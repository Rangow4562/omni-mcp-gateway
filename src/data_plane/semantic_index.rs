//! Hybrid Okapi BM25 + Dense Vector Reciprocal Rank Fusion (RRF) Semantic Index (v6.5.0).
//!
//! Combines:
//! - Deterministic 256-dimensional L2-normalized local embedding engine using stemmed tokens,
//!   curated domain synonym-concept expansion (so `"reimburse chargeback"` matches `"issue_refund"`
//!   even with zero lexical overlap), and character 3-gram / 4-gram signed feature hashing (< 200 µs).
//! - Optional remote HTTP embedding provider via `OMNI_EMBEDDING_URL` (OpenAI / Gemini / Ollama
//!   compatible JSON endpoint) with automatic fallback to local dense vectors on failure.
//! - Reciprocal Rank Fusion (RRF): `RRF(d) = w_bm25 / (60 + r_bm25(d)) + w_vec / (60 + r_vec(d))`.

use crate::data_plane::downstream_http::HttpDownstreamClient;
use crate::json::JsonValue;
use std::collections::{HashMap, HashSet};

pub const LOCAL_EMBEDDING_DIM: usize = 256;
pub const RRF_K: f64 = 60.0;

/// Built-in domain synonym concept clusters so semantically equivalent queries
/// project onto shared basis dimensions even when pure BM25 scores 0.0.
const SYNONYM_CLUSTERS: &[(&str, &[&str])] = &[
    (
        "concept_refund",
        &[
            "refund",
            "reimburse",
            "reimbursement",
            "chargeback",
            "credit",
            "payout",
            "dispute",
            "reversal",
            "return",
            "compensate",
            "compensation",
        ],
    ),
    (
        "concept_payment",
        &[
            "payment",
            "pay",
            "charge",
            "billing",
            "invoice",
            "receipt",
            "transaction",
            "ledger",
            "settlement",
            "checkout",
            "stripe",
            "wallet",
            "fare",
        ],
    ),
    (
        "concept_issue",
        &[
            "ticket",
            "issue",
            "bug",
            "incident",
            "defect",
            "jira",
            "linear",
            "alert",
            "escalation",
            "case",
            "task",
            "pr",
            "pullrequest",
        ],
    ),
    (
        "concept_deploy",
        &[
            "deploy",
            "deployment",
            "release",
            "rollout",
            "ship",
            "publish",
            "promote",
            "canary",
            "pipeline",
            "build",
            "kubernetes",
            "k8s",
        ],
    ),
    (
        "concept_user",
        &[
            "user",
            "customer",
            "rider",
            "driver",
            "account",
            "profile",
            "member",
            "identity",
            "principal",
            "person",
            "contact",
            "client",
        ],
    ),
    (
        "concept_search",
        &[
            "search", "find", "lookup", "query", "fetch", "retrieve", "list", "get", "discover",
            "inspect", "read", "browse",
        ],
    ),
    (
        "concept_delete",
        &[
            "delete",
            "remove",
            "purge",
            "destroy",
            "drop",
            "erase",
            "revoke",
            "cancel",
            "terminate",
            "archive",
            "cleanup",
        ],
    ),
    (
        "concept_notify",
        &[
            "notify",
            "notification",
            "message",
            "email",
            "slack",
            "sms",
            "page",
            "webhook",
            "broadcast",
            "send",
            "post",
            "chat",
        ],
    ),
    (
        "concept_database",
        &[
            "sql",
            "database",
            "postgres",
            "sqlite",
            "table",
            "schema",
            "row",
            "record",
            "store",
            "warehouse",
            "analytics",
            "metrics",
        ],
    ),
    (
        "concept_code",
        &[
            "repo",
            "repository",
            "git",
            "github",
            "commit",
            "branch",
            "diff",
            "file",
            "source",
            "code",
            "compile",
            "test",
        ],
    ),
    (
        "concept_ride",
        &[
            "ride", "trip", "booking", "dispatch", "route", "eta", "pickup", "dropoff", "vehicle",
            "order", "delivery", "shipment",
        ],
    ),
    (
        "concept_auth",
        &[
            "auth",
            "oauth",
            "token",
            "jwt",
            "secret",
            "credential",
            "permission",
            "rbac",
            "policy",
            "role",
            "grant",
            "audit",
        ],
    ),
];

/// Normalizes and tokenizes text into lowercase alphanumeric words, splitting camelCase and snake_case.
pub fn tokenize_semantic(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (idx, &ch) in chars.iter().enumerate() {
        if ch.is_ascii_alphanumeric() {
            // Split camelCase boundary (lower -> UPPER)
            if idx > 0
                && ch.is_ascii_uppercase()
                && chars[idx - 1].is_ascii_lowercase()
                && !cur.is_empty()
            {
                tokens.push(cur.to_ascii_lowercase());
                cur.clear();
            }
            cur.push(ch);
        } else if !cur.is_empty() {
            tokens.push(cur.to_ascii_lowercase());
            cur.clear();
        }
    }
    if !cur.is_empty() {
        tokens.push(cur.to_ascii_lowercase());
    }
    tokens.into_iter().filter(|t| t.len() >= 2).collect()
}

fn light_stem(word: &str) -> String {
    if word.len() <= 4 {
        return word.to_string();
    }
    for suffix in &[
        "ments", "ment", "ings", "ing", "tion", "ions", "ed", "es", "er", "s",
    ] {
        if let Some(stem) = word.strip_suffix(suffix) {
            if stem.len() >= 3 {
                return stem.to_string();
            }
        }
    }
    word.to_string()
}

fn fnv1a_u64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn add_hashed_feature(vec: &mut [f32; LOCAL_EMBEDDING_DIM], feature: &str, weight: f32) {
    let h1 = fnv1a_u64(feature.as_bytes());
    let h2 = fnv1a_u64(&h1.to_le_bytes());
    let idx1 = (h1 as usize) % LOCAL_EMBEDDING_DIM;
    let sign1 = if (h1 >> 63) == 0 { 1.0f32 } else { -1.0f32 };
    vec[idx1] += weight * sign1;

    let idx2 = (h2 as usize) % LOCAL_EMBEDDING_DIM;
    let sign2 = if (h2 >> 63) == 0 { 1.0f32 } else { -1.0f32 };
    vec[idx2] += (weight * 0.5) * sign2;
}

/// Computes a deterministic 256-dim L2-normalized local semantic embedding in `< 50 µs`.
pub fn embed_local(text: &str) -> Vec<f32> {
    let mut vec = [0.0f32; LOCAL_EMBEDDING_DIM];
    let tokens = tokenize_semantic(text);
    let mut active_concepts: HashSet<&'static str> = HashSet::new();

    for tok in &tokens {
        let stem = light_stem(tok);
        add_hashed_feature(&mut vec, &format!("w:{}", tok), 1.0);
        if stem != *tok {
            add_hashed_feature(&mut vec, &format!("s:{}", stem), 0.9);
        }

        // Expand synonym-concept clusters (strong semantic signal across non-overlapping terms)
        for &(concept_id, synonyms) in SYNONYM_CLUSTERS {
            if synonyms
                .iter()
                .any(|&syn| syn == tok.as_str() || syn == stem.as_str() || light_stem(syn) == stem)
            {
                active_concepts.insert(concept_id);
            }
        }

        // Character 3-grams and 4-grams for morphological/typo resilience
        let padded = format!("^{}$", tok);
        let bytes = padded.as_bytes();
        if bytes.len() >= 3 {
            for win in bytes.windows(3) {
                if let Ok(s) = std::str::from_utf8(win) {
                    add_hashed_feature(&mut vec, &format!("g3:{}", s), 0.35);
                }
            }
        }
        if bytes.len() >= 4 {
            for win in bytes.windows(4) {
                if let Ok(s) = std::str::from_utf8(win) {
                    add_hashed_feature(&mut vec, &format!("g4:{}", s), 0.25);
                }
            }
        }
    }

    for concept in active_concepts {
        add_hashed_feature(&mut vec, &format!("c:{}", concept), 2.5);
    }

    normalize_l2(&mut vec);
    vec.to_vec()
}

fn normalize_l2(vec: &mut [f32]) {
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    if norm_sq > 1e-12 {
        let inv = 1.0 / norm_sq.sqrt();
        for v in vec.iter_mut() {
            *v *= inv;
        }
    }
}

/// Computes cosine similarity between two L2-normalized vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| (x as f64) * (y as f64))
        .sum();
    dot.clamp(-1.0, 1.0)
}

/// Optional remote HTTP embedding client (`OMNI_EMBEDDING_URL`) with automatic local fallback.
pub fn embed_text(text: &str, remote_endpoint: Option<&str>) -> Vec<f32> {
    if let Some(url) = remote_endpoint.filter(|u| !u.trim().is_empty()) {
        if let Ok(mut remote_vec) = fetch_remote_embedding(url, text) {
            if !remote_vec.is_empty() {
                normalize_l2(&mut remote_vec);
                return remote_vec;
            }
        }
    }
    embed_local(text)
}

fn fetch_remote_embedding(url: &str, text: &str) -> Result<Vec<f32>, String> {
    let client = HttpDownstreamClient::new();
    let model = std::env::var("OMNI_EMBEDDING_MODEL")
        .unwrap_or_else(|_| "text-embedding-3-small".to_string());
    let body = json!({
        "model": model,
        "input": text
    });
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    if let Ok(api_key) = std::env::var("OMNI_EMBEDDING_API_KEY") {
        if !api_key.trim().is_empty() {
            headers.insert(
                "Authorization".to_string(),
                format!("Bearer {}", api_key.trim()),
            );
        }
    }
    let resp = client.post_json(url, body.to_string_compact().as_bytes(), Some(&headers))?;
    let resp_str = String::from_utf8(resp).map_err(|e| e.to_string())?;
    let parsed = JsonValue::parse(&resp_str)?;

    // Support OpenAI `{ data: [{ embedding: [...] }] }` and Ollama `{ embedding: [...] }`
    let arr_opt = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("embedding"))
        .and_then(|e| e.as_array())
        .or_else(|| parsed.get("embedding").and_then(|e| e.as_array()));

    let arr = arr_opt.ok_or_else(|| "Missing embedding array in response".to_string())?;
    let vec: Vec<f32> = arr
        .iter()
        .filter_map(|v| v.as_f64().map(|n| n as f32))
        .collect();
    if vec.is_empty() {
        return Err("Empty embedding vector returned".to_string());
    }
    Ok(vec)
}

/// A document indexed for Hybrid BM25 + Dense Vector RRF ranking.
#[derive(Clone, Debug)]
pub struct HybridDocument {
    pub id: String,
    pub text: String,
    pub bm25_score: f64,
}

/// Ranked result emitted by Hybrid RRF fusion.
#[derive(Clone, Debug)]
pub struct HybridRankedItem {
    pub id: String,
    pub bm25_score: f64,
    pub vector_similarity: f64,
    pub rrf_score: f64,
}

/// Fuses Okapi BM25 scores and Dense Vector cosine similarity via Reciprocal Rank Fusion (RRF):
/// `RRF(d) = w_bm25 / (60 + r_bm25(d)) + w_vec / (60 + r_vec(d))`
/// Only documents with positive BM25 score or vector similarity >= `min_vector_sim` contribute rank.
pub fn rank_hybrid_rrf(
    query: &str,
    docs: &[HybridDocument],
    w_bm25: f64,
    w_vec: f64,
    min_vector_sim: f64,
    remote_embedding_url: Option<&str>,
) -> Vec<HybridRankedItem> {
    if docs.is_empty() {
        return Vec::new();
    }

    let q_vec = embed_text(query, remote_embedding_url);

    // 1. Compute dense cosine similarity for each document
    let mut vec_scores: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let d_vec = embed_text(&d.text, remote_embedding_url);
            (i, cosine_similarity(&q_vec, &d_vec))
        })
        .collect();

    // 2. Rank by BM25 descending (only positive BM25 scores earn a BM25 rank)
    let mut bm25_indices: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.bm25_score > 0.0)
        .map(|(i, d)| (i, d.bm25_score))
        .collect();
    bm25_indices.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| docs[a.0].id.cmp(&docs[b.0].id))
    });
    let mut bm25_rank: HashMap<usize, usize> = HashMap::new();
    for (rank_zero, (doc_idx, _)) in bm25_indices.iter().enumerate() {
        bm25_rank.insert(*doc_idx, rank_zero + 1);
    }

    // 3. Rank by Vector cosine similarity descending (only >= min_vector_sim earn vector rank)
    vec_scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| docs[a.0].id.cmp(&docs[b.0].id))
    });
    let mut vec_rank: HashMap<usize, usize> = HashMap::new();
    let mut sim_by_idx: HashMap<usize, f64> = HashMap::new();
    for (rank_zero, &(doc_idx, sim)) in vec_scores.iter().enumerate() {
        sim_by_idx.insert(doc_idx, sim);
        if sim >= min_vector_sim {
            vec_rank.insert(doc_idx, rank_zero + 1);
        }
    }

    // 4. Fuse via RRF
    let mut out: Vec<HybridRankedItem> = Vec::new();
    for (idx, doc) in docs.iter().enumerate() {
        let sim = sim_by_idx.get(&idx).copied().unwrap_or(0.0);
        let r_bm25 = bm25_rank.get(&idx).copied();
        let r_vec = vec_rank.get(&idx).copied();
        if r_bm25.is_none() && r_vec.is_none() {
            continue;
        }
        let mut rrf = 0.0;
        if let Some(rb) = r_bm25 {
            rrf += w_bm25 / (RRF_K + rb as f64);
        }
        if let Some(rv) = r_vec {
            // Modulate vector RRF slightly by cosine similarity so strong semantic matches clearly outrank marginal ones
            rrf += (w_vec / (RRF_K + rv as f64)) * (0.5 + 0.5 * sim.max(0.0));
        }
        out.push(HybridRankedItem {
            id: doc.id.clone(),
            bm25_score: doc.bm25_score,
            vector_similarity: sim,
            rrf_score: rrf,
        });
    }

    out.sort_by(|a, b| {
        b.rrf_score
            .partial_cmp(&a.rrf_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.vector_similarity
                    .partial_cmp(&a.vector_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}
