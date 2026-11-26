//! Universal Multi-Cloud File Blob Storage Engine (`io.modelcontextprotocol/files`, SEP-2356, v6.8.0).
//!
//! Pure `#![forbid(unsafe_code)]` Rust with zero vendor SDK crates, supporting:
//! - `local` (default): Local disk, Docker volume, NFS, or Kubernetes `ReadWriteMany` PVC
//! - `postgres`: Chunked `BYTEA` in `omni_file_blob_chunks` (works over Postgres/SQLite/WAL)
//! - `s3`: AWS SigV4 (AWS S3, Cloudflare R2, MinIO, DigitalOcean Spaces, Backblaze B2, Ceph RGW, OCI)
//! - `gcs`: Google Cloud Storage native REST API (Bearer OAuth2 / Service Account token)
//! - `azure`: Microsoft Azure Blob Storage REST API (SharedKey HMAC-SHA256 / SAS / Bearer)
//! - `http`: Generic HTTP / WebDAV `PUT`/`GET`/`Range`/`DELETE` blob proxy.

use crate::config::OmniConfig;
use crate::control_plane::MCPRegistry;
use crate::logging::now_epoch_secs;
use crate::models::{constant_time_eq, continuation_key, generate_ulid, FileBlobRecord};
use crate::storage::audit_log::{hmac_sha256, sha256_hex};
use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BLOB_CHUNK_BYTES: usize = 256 * 1024;

/// Universal cloud-agnostic blob storage backend trait.
pub trait FileBlobBackend: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn put_bytes(&self, storage_key: &str, data: &[u8], content_type: &str) -> Result<(), String>;
    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String>;
    fn get_range(&self, storage_key: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let all = self.get_bytes(storage_key)?;
        let start = (offset as usize).min(all.len());
        let end = start.saturating_add(len).min(all.len());
        Ok(all[start..end].to_vec())
    }
    fn delete(&self, storage_key: &str) -> Result<(), String>;
}

// ============================================================================
// 1. Local / Shared POSIX Backend
// ============================================================================

pub struct LocalFileBlobBackend {
    base_dir: PathBuf,
}

impl LocalFileBlobBackend {
    pub fn new(base_dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&base_dir);
        Self { base_dir }
    }

    fn resolve_path(&self, storage_key: &str) -> Result<PathBuf, String> {
        if storage_key.is_empty()
            || storage_key.contains("..")
            || storage_key.contains('/')
            || storage_key.contains('\\')
        {
            return Err(format!("Invalid local blob storage key '{}'", storage_key));
        }
        Ok(self.base_dir.join(storage_key))
    }
}

impl FileBlobBackend for LocalFileBlobBackend {
    fn backend_name(&self) -> &'static str {
        "local"
    }

    fn put_bytes(&self, storage_key: &str, data: &[u8], _content_type: &str) -> Result<(), String> {
        let _ = fs::create_dir_all(&self.base_dir);
        let path = self.resolve_path(storage_key)?;
        let tmp = path.with_extension("tmp");
        let mut f = File::create(&tmp).map_err(|e| format!("Failed to create blob file: {}", e))?;
        f.write_all(data)
            .map_err(|e| format!("Failed to write blob file: {}", e))?;
        f.flush()
            .map_err(|e| format!("Failed to flush blob file: {}", e))?;
        drop(f);
        fs::rename(&tmp, &path).map_err(|e| format!("Failed to commit blob file: {}", e))?;
        Ok(())
    }

    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String> {
        let path = self.resolve_path(storage_key)?;
        fs::read(&path).map_err(|e| format!("Failed to read blob '{}': {}", storage_key, e))
    }

    fn get_range(&self, storage_key: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let path = self.resolve_path(storage_key)?;
        let mut f = File::open(&path)
            .map_err(|e| format!("Failed to open blob '{}': {}", storage_key, e))?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("Failed to seek blob '{}': {}", storage_key, e))?;
        let mut buf = vec![0u8; len];
        let mut read_total = 0usize;
        while read_total < len {
            let n = f
                .read(&mut buf[read_total..])
                .map_err(|e| format!("Failed to read blob range: {}", e))?;
            if n == 0 {
                break;
            }
            read_total += n;
        }
        buf.truncate(read_total);
        Ok(buf)
    }

    fn delete(&self, storage_key: &str) -> Result<(), String> {
        let path = self.resolve_path(storage_key)?;
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("Failed to delete blob: {}", e))?;
        }
        Ok(())
    }
}

// ============================================================================
// 2. PostgreSQL / SQLite / WAL Chunked BYTEA Backend
// ============================================================================

pub struct PostgresFileBlobBackend {
    registry: MCPRegistry,
}

impl PostgresFileBlobBackend {
    pub fn new(registry: MCPRegistry) -> Self {
        Self { registry }
    }
}

impl FileBlobBackend for PostgresFileBlobBackend {
    fn backend_name(&self) -> &'static str {
        "postgres"
    }

    fn put_bytes(&self, storage_key: &str, data: &[u8], _content_type: &str) -> Result<(), String> {
        let backend = self.registry.backend().ok_or_else(|| {
            "No durable RegistryStorageBackend attached for postgres file storage".to_string()
        })?;
        backend.delete_file_blob_chunks(storage_key);
        if data.is_empty() {
            backend.save_file_blob_chunk(storage_key, 0, &[])?;
            return Ok(());
        }
        for (idx, chunk) in data.chunks(BLOB_CHUNK_BYTES).enumerate() {
            backend.save_file_blob_chunk(storage_key, idx as u32, chunk)?;
        }
        Ok(())
    }

    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String> {
        let backend = self
            .registry
            .backend()
            .ok_or_else(|| "No durable RegistryStorageBackend attached".to_string())?;
        let chunks = backend.load_file_blob_chunks(storage_key)?;
        let total: usize = chunks.iter().map(|c| c.len()).sum();
        let mut out = Vec::with_capacity(total);
        for c in chunks {
            out.extend_from_slice(&c);
        }
        Ok(out)
    }

    fn delete(&self, storage_key: &str) -> Result<(), String> {
        if let Some(backend) = self.registry.backend() {
            backend.delete_file_blob_chunks(storage_key);
        }
        Ok(())
    }
}

// ============================================================================
// 3. AWS S3 / S3-Compatible (SigV4) Backend
// ============================================================================

#[derive(Clone, Debug)]
pub struct S3BlobConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub path_style: bool,
}

impl S3BlobConfig {
    pub fn from_env() -> Self {
        let region = env::var("OMNI_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let bucket = env::var("OMNI_S3_BUCKET").unwrap_or_else(|_| "omni-mcp-blobs".to_string());
        let endpoint = env::var("OMNI_S3_ENDPOINT")
            .unwrap_or_else(|_| format!("https://s3.{}.amazonaws.com", region));
        let path_style = env::var("OMNI_S3_PATH_STYLE")
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(true);
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            bucket,
            region,
            access_key_id: env::var("OMNI_S3_ACCESS_KEY_ID").unwrap_or_default(),
            secret_access_key: env::var("OMNI_S3_SECRET_ACCESS_KEY").unwrap_or_default(),
            path_style,
        }
    }
}

pub struct S3FileBlobBackend {
    pub cfg: S3BlobConfig,
}

impl S3FileBlobBackend {
    pub fn new(cfg: S3BlobConfig) -> Self {
        Self { cfg }
    }

    /// Computes AWS Signature Version 4 `Authorization` header, `x-amz-date`, and `x-amz-content-sha256`.
    pub fn sign_sigv4(
        &self,
        method: &str,
        host: &str,
        canonical_uri: &str,
        payload_sha256: &str,
        amz_date: &str,
    ) -> String {
        let date_stamp = &amz_date[..8];
        let canonical_headers = format!(
            "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
            host, payload_sha256, amz_date
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method, canonical_uri, canonical_headers, signed_headers, payload_sha256
        );
        let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
        let credential_scope = format!("{}/{}/s3/aws4_request", date_stamp, self.cfg.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, canonical_request_hash
        );

        let k_secret = format!("AWS4{}", self.cfg.secret_access_key);
        let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
        let k_region = hmac_sha256(&k_date, self.cfg.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let sig_bytes = hmac_sha256(&k_signing, string_to_sign.as_bytes());
        let signature = hex_encode(&sig_bytes);

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.cfg.access_key_id, credential_scope, signed_headers, signature
        )
    }

    fn build_url_and_uri(&self, storage_key: &str) -> (String, String) {
        if self.cfg.path_style {
            let uri = format!("/{}/{}", self.cfg.bucket, storage_key);
            (format!("{}{}", self.cfg.endpoint, uri), uri)
        } else {
            let uri = format!("/{}", storage_key);
            let (scheme, rest) = self
                .cfg
                .endpoint
                .split_once("://")
                .unwrap_or(("https", &self.cfg.endpoint));
            (
                format!("{}://{}.{}{}", scheme, self.cfg.bucket, rest, uri),
                uri,
            )
        }
    }
}

impl FileBlobBackend for S3FileBlobBackend {
    fn backend_name(&self) -> &'static str {
        "s3"
    }

    fn put_bytes(&self, storage_key: &str, data: &[u8], content_type: &str) -> Result<(), String> {
        let (url, uri) = self.build_url_and_uri(storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let payload_hash = sha256_hex(data);
        let amz_date = utc_amz_date();
        let auth = self.sign_sigv4("PUT", &parsed.host_header, &uri, &payload_hash, &amz_date);
        let headers = vec![
            ("Authorization".to_string(), auth),
            ("x-amz-date".to_string(), amz_date),
            ("x-amz-content-sha256".to_string(), payload_hash),
            ("Content-Type".to_string(), content_type.to_string()),
        ];
        let (status, _) = http_blob_request(&parsed, "PUT", &headers, data)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("S3 PUT {} failed with HTTP {}", url, status))
        }
    }

    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String> {
        let (url, uri) = self.build_url_and_uri(storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let payload_hash = sha256_hex(b"");
        let amz_date = utc_amz_date();
        let auth = self.sign_sigv4("GET", &parsed.host_header, &uri, &payload_hash, &amz_date);
        let headers = vec![
            ("Authorization".to_string(), auth),
            ("x-amz-date".to_string(), amz_date),
            ("x-amz-content-sha256".to_string(), payload_hash),
        ];
        let (status, body) = http_blob_request(&parsed, "GET", &headers, b"")?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!("S3 GET {} failed with HTTP {}", url, status))
        }
    }

    fn get_range(&self, storage_key: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let (url, uri) = self.build_url_and_uri(storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let payload_hash = sha256_hex(b"");
        let amz_date = utc_amz_date();
        let auth = self.sign_sigv4("GET", &parsed.host_header, &uri, &payload_hash, &amz_date);
        let end = offset.saturating_add(len as u64).saturating_sub(1);
        let headers = vec![
            ("Authorization".to_string(), auth),
            ("x-amz-date".to_string(), amz_date),
            ("x-amz-content-sha256".to_string(), payload_hash),
            ("Range".to_string(), format!("bytes={}-{}", offset, end)),
        ];
        let (status, body) = http_blob_request(&parsed, "GET", &headers, b"")?;
        if status == 206 || status == 200 {
            if status == 200 {
                let s = (offset as usize).min(body.len());
                let e = s.saturating_add(len).min(body.len());
                return Ok(body[s..e].to_vec());
            }
            Ok(body)
        } else {
            Err(format!("S3 GET Range {} failed with HTTP {}", url, status))
        }
    }

    fn delete(&self, storage_key: &str) -> Result<(), String> {
        let (url, uri) = self.build_url_and_uri(storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let payload_hash = sha256_hex(b"");
        let amz_date = utc_amz_date();
        let auth = self.sign_sigv4(
            "DELETE",
            &parsed.host_header,
            &uri,
            &payload_hash,
            &amz_date,
        );
        let headers = vec![
            ("Authorization".to_string(), auth),
            ("x-amz-date".to_string(), amz_date),
            ("x-amz-content-sha256".to_string(), payload_hash),
        ];
        let (status, _) = http_blob_request(&parsed, "DELETE", &headers, b"")?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(format!("S3 DELETE {} failed with HTTP {}", url, status))
        }
    }
}

// ============================================================================
// 4. Google Cloud Storage (GCS) Native REST Backend
// ============================================================================

#[derive(Clone, Debug)]
pub struct GcsBlobConfig {
    pub endpoint: String,
    pub bucket: String,
    pub bearer_token: String,
    pub sa_key_file: Option<String>,
}

impl GcsBlobConfig {
    pub fn from_env() -> Self {
        let sa_key_file = env::var("OMNI_GCS_SA_KEY_FILE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut bearer_token = env::var("OMNI_GCS_TOKEN").unwrap_or_default();
        if bearer_token.is_empty() {
            if let Some(ref key_path) = sa_key_file {
                match exchange_gcs_sa_key_file(key_path) {
                    Ok(tok) => bearer_token = tok,
                    Err(e) => eprintln!("[!] OMNI_GCS_SA_KEY_FILE token exchange failed: {}", e),
                }
            }
        }
        Self {
            endpoint: env::var("OMNI_GCS_ENDPOINT")
                .unwrap_or_else(|_| "https://storage.googleapis.com".to_string())
                .trim_end_matches('/')
                .to_string(),
            bucket: env::var("OMNI_GCS_BUCKET").unwrap_or_else(|_| "omni-mcp-blobs".to_string()),
            bearer_token,
            sa_key_file,
        }
    }
}

pub struct GcsFileBlobBackend {
    pub cfg: GcsBlobConfig,
}

impl GcsFileBlobBackend {
    pub fn new(cfg: GcsBlobConfig) -> Self {
        Self { cfg }
    }

    fn object_url(&self, storage_key: &str) -> String {
        format!("{}/{}/{}", self.cfg.endpoint, self.cfg.bucket, storage_key)
    }

    fn auth_headers(&self) -> Vec<(String, String)> {
        if self.cfg.bearer_token.is_empty() {
            Vec::new()
        } else {
            vec![(
                "Authorization".to_string(),
                format!("Bearer {}", self.cfg.bearer_token),
            )]
        }
    }
}

impl FileBlobBackend for GcsFileBlobBackend {
    fn backend_name(&self) -> &'static str {
        "gcs"
    }

    fn put_bytes(&self, storage_key: &str, data: &[u8], content_type: &str) -> Result<(), String> {
        let parsed = ParsedHttpUrl::parse(&self.object_url(storage_key))?;
        let mut headers = self.auth_headers();
        headers.push(("Content-Type".to_string(), content_type.to_string()));
        let (status, _) = http_blob_request(&parsed, "PUT", &headers, data)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("GCS PUT failed with HTTP {}", status))
        }
    }

    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String> {
        let parsed = ParsedHttpUrl::parse(&self.object_url(storage_key))?;
        let (status, body) = http_blob_request(&parsed, "GET", &self.auth_headers(), b"")?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!("GCS GET failed with HTTP {}", status))
        }
    }

    fn get_range(&self, storage_key: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let parsed = ParsedHttpUrl::parse(&self.object_url(storage_key))?;
        let mut headers = self.auth_headers();
        let end = offset.saturating_add(len as u64).saturating_sub(1);
        headers.push(("Range".to_string(), format!("bytes={}-{}", offset, end)));
        let (status, body) = http_blob_request(&parsed, "GET", &headers, b"")?;
        if status == 206 || status == 200 {
            if status == 200 {
                let s = (offset as usize).min(body.len());
                let e = s.saturating_add(len).min(body.len());
                return Ok(body[s..e].to_vec());
            }
            Ok(body)
        } else {
            Err(format!("GCS GET Range failed with HTTP {}", status))
        }
    }

    fn delete(&self, storage_key: &str) -> Result<(), String> {
        let parsed = ParsedHttpUrl::parse(&self.object_url(storage_key))?;
        let (status, _) = http_blob_request(&parsed, "DELETE", &self.auth_headers(), b"")?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(format!("GCS DELETE failed with HTTP {}", status))
        }
    }
}

// ============================================================================
// 5. Microsoft Azure Blob Storage (SharedKey HMAC-SHA256 / SAS / Bearer)
// ============================================================================

#[derive(Clone, Debug)]
pub struct AzureBlobConfig {
    pub endpoint: String,
    pub account: String,
    pub container: String,
    pub shared_key_base64: Option<String>,
    pub sas_token: Option<String>,
    pub bearer_token: Option<String>,
}

impl AzureBlobConfig {
    pub fn from_env() -> Self {
        let account =
            env::var("OMNI_AZURE_STORAGE_ACCOUNT").unwrap_or_else(|_| "omnimcp".to_string());
        let container =
            env::var("OMNI_AZURE_CONTAINER").unwrap_or_else(|_| "omni-mcp-blobs".to_string());
        let endpoint = env::var("OMNI_AZURE_ENDPOINT")
            .unwrap_or_else(|_| format!("https://{}.blob.core.windows.net", account))
            .trim_end_matches('/')
            .to_string();
        Self {
            endpoint,
            account,
            container,
            shared_key_base64: env::var("OMNI_AZURE_SHARED_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            sas_token: env::var("OMNI_AZURE_SAS_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            bearer_token: env::var("OMNI_AZURE_BEARER_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}

pub struct AzureFileBlobBackend {
    pub cfg: AzureBlobConfig,
}

impl AzureFileBlobBackend {
    pub fn new(cfg: AzureBlobConfig) -> Self {
        Self { cfg }
    }

    /// Computes Azure Blob Storage `SharedKey` HMAC-SHA256 Authorization header.
    pub fn sign_shared_key(
        &self,
        method: &str,
        content_length: Option<usize>,
        content_type: &str,
        x_ms_date: &str,
        x_ms_blob_type: Option<&str>,
        storage_key: &str,
    ) -> Result<String, String> {
        let key_b64 = self
            .cfg
            .shared_key_base64
            .as_deref()
            .ok_or_else(|| "OMNI_AZURE_SHARED_KEY not configured".to_string())?;
        let key_bytes = base64_decode(key_b64)
            .ok_or_else(|| "Invalid base64 in OMNI_AZURE_SHARED_KEY".to_string())?;
        let len_str = content_length
            .filter(|&n| n > 0)
            .map(|n| n.to_string())
            .unwrap_or_default();
        let mut canonical_headers = String::new();
        if let Some(bt) = x_ms_blob_type {
            canonical_headers.push_str(&format!("x-ms-blob-type:{}\n", bt));
        }
        canonical_headers.push_str(&format!("x-ms-date:{}\n", x_ms_date));
        canonical_headers.push_str("x-ms-version:2023-11-03\n");
        let canonical_resource = format!(
            "/{}/{}/{}",
            self.cfg.account, self.cfg.container, storage_key
        );
        let string_to_sign = format!(
            "{}\n\n\n{}\n\n{}\n\n\n\n\n\n\n{}{}",
            method, len_str, content_type, canonical_headers, canonical_resource
        );
        let sig = hmac_sha256(&key_bytes, string_to_sign.as_bytes());
        Ok(format!(
            "SharedKey {}:{}",
            self.cfg.account,
            base64_encode(&sig)
        ))
    }

    fn blob_url(&self, storage_key: &str) -> String {
        let base = format!(
            "{}/{}/{}",
            self.cfg.endpoint, self.cfg.container, storage_key
        );
        if let Some(ref sas) = self.cfg.sas_token {
            format!("{}?{}", base, sas.trim_start_matches('?'))
        } else {
            base
        }
    }

    fn build_headers(
        &self,
        method: &str,
        content_length: Option<usize>,
        content_type: &str,
        blob_type: Option<&str>,
        storage_key: &str,
    ) -> Result<Vec<(String, String)>, String> {
        let date = utc_rfc1123_date();
        let mut headers = vec![
            ("x-ms-date".to_string(), date.clone()),
            ("x-ms-version".to_string(), "2023-11-03".to_string()),
        ];
        if let Some(bt) = blob_type {
            headers.push(("x-ms-blob-type".to_string(), bt.to_string()));
        }
        if !content_type.is_empty() {
            headers.push(("Content-Type".to_string(), content_type.to_string()));
        }
        if let Some(ref bearer) = self.cfg.bearer_token {
            headers.push(("Authorization".to_string(), format!("Bearer {}", bearer)));
        } else if self.cfg.shared_key_base64.is_some() {
            let auth = self.sign_shared_key(
                method,
                content_length,
                content_type,
                &date,
                blob_type,
                storage_key,
            )?;
            headers.push(("Authorization".to_string(), auth));
        }
        Ok(headers)
    }
}

impl FileBlobBackend for AzureFileBlobBackend {
    fn backend_name(&self) -> &'static str {
        "azure"
    }

    fn put_bytes(&self, storage_key: &str, data: &[u8], content_type: &str) -> Result<(), String> {
        let parsed = ParsedHttpUrl::parse(&self.blob_url(storage_key))?;
        let headers = self.build_headers(
            "PUT",
            Some(data.len()),
            content_type,
            Some("BlockBlob"),
            storage_key,
        )?;
        let (status, _) = http_blob_request(&parsed, "PUT", &headers, data)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("Azure PUT failed with HTTP {}", status))
        }
    }

    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String> {
        let parsed = ParsedHttpUrl::parse(&self.blob_url(storage_key))?;
        let headers = self.build_headers("GET", None, "", None, storage_key)?;
        let (status, body) = http_blob_request(&parsed, "GET", &headers, b"")?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!("Azure GET failed with HTTP {}", status))
        }
    }

    fn delete(&self, storage_key: &str) -> Result<(), String> {
        let parsed = ParsedHttpUrl::parse(&self.blob_url(storage_key))?;
        let headers = self.build_headers("DELETE", None, "", None, storage_key)?;
        let (status, _) = http_blob_request(&parsed, "DELETE", &headers, b"")?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(format!("Azure DELETE failed with HTTP {}", status))
        }
    }
}

// ============================================================================
// 6. Generic HTTP / WebDAV Backend
// ============================================================================

#[derive(Clone, Debug)]
pub struct HttpBlobConfig {
    pub base_url: String,
    pub auth_header: Option<String>,
}

impl HttpBlobConfig {
    pub fn from_env() -> Self {
        let raw_auth = env::var("OMNI_HTTP_BLOB_AUTH")
            .ok()
            .filter(|s| !s.is_empty());
        let resolved_auth = raw_auth.map(|val| {
            if let Some(env_name) = val.strip_prefix("{env:").and_then(|s| s.strip_suffix('}')) {
                env::var(env_name).unwrap_or_default()
            } else {
                val
            }
        });
        Self {
            base_url: env::var("OMNI_HTTP_BLOB_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:9000/blobs".to_string())
                .trim_end_matches('/')
                .to_string(),
            auth_header: resolved_auth.filter(|s| !s.is_empty()),
        }
    }
}

pub struct HttpFileBlobBackend {
    pub cfg: HttpBlobConfig,
}

impl HttpFileBlobBackend {
    pub fn new(cfg: HttpBlobConfig) -> Self {
        Self { cfg }
    }

    fn headers(&self, content_type: Option<&str>) -> Vec<(String, String)> {
        let mut h = Vec::new();
        if let Some(ref auth) = self.cfg.auth_header {
            h.push(("Authorization".to_string(), auth.clone()));
        }
        if let Some(ct) = content_type {
            h.push(("Content-Type".to_string(), ct.to_string()));
        }
        h
    }
}

impl FileBlobBackend for HttpFileBlobBackend {
    fn backend_name(&self) -> &'static str {
        "http"
    }

    fn put_bytes(&self, storage_key: &str, data: &[u8], content_type: &str) -> Result<(), String> {
        let url = format!("{}/{}", self.cfg.base_url, storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let (status, _) =
            http_blob_request(&parsed, "PUT", &self.headers(Some(content_type)), data)?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("HTTP blob PUT {} failed with HTTP {}", url, status))
        }
    }

    fn get_bytes(&self, storage_key: &str) -> Result<Vec<u8>, String> {
        let url = format!("{}/{}", self.cfg.base_url, storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let (status, body) = http_blob_request(&parsed, "GET", &self.headers(None), b"")?;
        if (200..300).contains(&status) {
            Ok(body)
        } else {
            Err(format!("HTTP blob GET {} failed with HTTP {}", url, status))
        }
    }

    fn get_range(&self, storage_key: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let url = format!("{}/{}", self.cfg.base_url, storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let mut headers = self.headers(None);
        let end = offset.saturating_add(len as u64).saturating_sub(1);
        headers.push(("Range".to_string(), format!("bytes={}-{}", offset, end)));
        let (status, body) = http_blob_request(&parsed, "GET", &headers, b"")?;
        if status == 206 || status == 200 {
            if status == 200 {
                let s = (offset as usize).min(body.len());
                let e = s.saturating_add(len).min(body.len());
                return Ok(body[s..e].to_vec());
            }
            Ok(body)
        } else {
            Err(format!(
                "HTTP blob Range {} failed with HTTP {}",
                url, status
            ))
        }
    }

    fn delete(&self, storage_key: &str) -> Result<(), String> {
        let url = format!("{}/{}", self.cfg.base_url, storage_key);
        let parsed = ParsedHttpUrl::parse(&url)?;
        let (status, _) = http_blob_request(&parsed, "DELETE", &self.headers(None), b"")?;
        if (200..300).contains(&status) || status == 404 {
            Ok(())
        } else {
            Err(format!(
                "HTTP blob DELETE {} failed with HTTP {}",
                url, status
            ))
        }
    }
}

// ============================================================================
// High-Level FileBlobStore Facade (Metadata + HMAC Tokens + Multi-Cloud Backend)
// ============================================================================

#[derive(Clone)]
pub struct FileBlobStore {
    pub backend: Arc<dyn FileBlobBackend>,
    pub registry: MCPRegistry,
    pub max_file_bytes: usize,
    pub ttl_sec: u64,
}

impl FileBlobStore {
    pub fn from_config(config: &OmniConfig, registry: MCPRegistry) -> Self {
        let backend: Arc<dyn FileBlobBackend> = match config.file_storage_backend.as_str() {
            "postgres" | "postgresql" | "db" => {
                Arc::new(PostgresFileBlobBackend::new(registry.clone()))
            }
            "s3" => Arc::new(S3FileBlobBackend::new(S3BlobConfig::from_env())),
            "gcs" => Arc::new(GcsFileBlobBackend::new(GcsBlobConfig::from_env())),
            "azure" => Arc::new(AzureFileBlobBackend::new(AzureBlobConfig::from_env())),
            "http" | "webdav" => Arc::new(HttpFileBlobBackend::new(HttpBlobConfig::from_env())),
            _ => Arc::new(LocalFileBlobBackend::new(config.uploads_dir.clone())),
        };
        Self {
            backend,
            registry,
            max_file_bytes: config.file_upload_max_bytes,
            ttl_sec: config.file_upload_ttl_sec,
        }
    }

    pub fn with_backend(
        backend: Arc<dyn FileBlobBackend>,
        registry: MCPRegistry,
        max_file_bytes: usize,
        ttl_sec: u64,
    ) -> Self {
        Self {
            backend,
            registry,
            max_file_bytes,
            ttl_sec,
        }
    }

    /// Generates an HMAC-SHA256 signed upload/download token scoped to `(purpose, id)`.
    pub fn sign_token(purpose: &str, id: &str) -> String {
        let msg = format!("omni-file:{}:{}", purpose, id);
        let mac = hmac_sha256(continuation_key().as_bytes(), msg.as_bytes());
        hex_encode(&mac)
    }

    pub fn verify_token(purpose: &str, id: &str, token: &str) -> bool {
        let expected = Self::sign_token(purpose, id);
        constant_time_eq(expected.as_bytes(), token.as_bytes())
    }

    /// Ingests an uploaded file payload, verifies size cap, computes SHA-256, stores in the
    /// configured `FileBlobBackend`, and persists metadata in `omni_file_blobs`.
    pub fn put_file(
        &self,
        file_id_opt: Option<String>,
        filename: &str,
        mime_type: &str,
        caller_actor: &str,
        data: &[u8],
    ) -> Result<FileBlobRecord, String> {
        if data.len() > self.max_file_bytes {
            return Err(format!(
                "Uploaded file size {} bytes exceeds maximum allowed {} bytes",
                data.len(),
                self.max_file_bytes
            ));
        }
        let file_id = file_id_opt
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("file_{}", generate_ulid()));
        let storage_key = format!("{}.blob", file_id);
        let sha256 = sha256_hex(data);
        self.backend.put_bytes(&storage_key, data, mime_type)?;

        let now = now_epoch_secs();
        let rec = FileBlobRecord {
            file_id,
            filename: filename.to_string(),
            mime_type: mime_type.to_string(),
            size_bytes: data.len() as u64,
            sha256,
            backend: self.backend.backend_name().to_string(),
            storage_key,
            caller_actor: caller_actor.to_string(),
            created_at: now,
            expires_at: now + self.ttl_sec as f64,
        };
        self.registry.save_file_blob(&rec);
        Ok(rec)
    }

    pub fn get_metadata(&self, file_id: &str) -> Option<FileBlobRecord> {
        let rec = self.registry.get_file_blob(file_id)?;
        if now_epoch_secs() > rec.expires_at {
            let _ = self.delete_file(file_id);
            return None;
        }
        Some(rec)
    }

    pub fn read_all(&self, file_id: &str) -> Result<(FileBlobRecord, Vec<u8>), String> {
        let rec = self
            .get_metadata(file_id)
            .ok_or_else(|| format!("File '{}' not found or expired", file_id))?;
        let bytes = self.backend.get_bytes(&rec.storage_key)?;
        Ok((rec, bytes))
    }

    pub fn read_range(&self, file_id: &str, offset: u64, len: usize) -> Result<Vec<u8>, String> {
        let rec = self
            .get_metadata(file_id)
            .ok_or_else(|| format!("File '{}' not found or expired", file_id))?;
        self.backend.get_range(&rec.storage_key, offset, len)
    }

    pub fn delete_file(&self, file_id: &str) -> Option<FileBlobRecord> {
        let removed = self.registry.delete_file_blob(file_id)?;
        let _ = self.backend.delete(&removed.storage_key);
        Some(removed)
    }

    pub fn purge_expired(&self) -> usize {
        let expired = self.registry.purge_expired_file_blobs(now_epoch_secs());
        for rec in &expired {
            let _ = self.backend.delete(&rec.storage_key);
        }
        expired.len()
    }
}

// ============================================================================
// Minimal Pure-Rust HTTP/1.1 Blob Client (TCP + optional rustls TLS)
// ============================================================================

struct ParsedHttpUrl {
    https: bool,
    host: String,
    port: u16,
    host_header: String,
    path_and_query: String,
}

impl ParsedHttpUrl {
    fn parse(url: &str) -> Result<Self, String> {
        let (https, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(format!("Unsupported blob URL scheme: {}", url));
        };
        let (authority, path_and_query) = match rest.find('/') {
            Some(idx) => (&rest[..idx], rest[idx..].to_string()),
            None => (rest, "/".to_string()),
        };
        let default_port = if https { 443 } else { 80 };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(default_port)),
            None => (authority.to_string(), default_port),
        };
        let host_header = if port == default_port {
            host.clone()
        } else {
            format!("{}:{}", host, port)
        };
        Ok(Self {
            https,
            host,
            port,
            host_header,
            path_and_query,
        })
    }
}

fn http_blob_request(
    url: &ParsedHttpUrl,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        method,
        url.path_and_query,
        url.host_header,
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str("\r\n");

    let addr = format!("{}:{}", url.host, url.port);
    let mut tcp = TcpStream::connect(&addr)
        .map_err(|e| format!("Failed to connect to blob endpoint {}: {}", addr, e))?;
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(30)));

    let mut raw_resp = Vec::new();
    if url.https {
        #[cfg(feature = "tls-rustls")]
        {
            let mut root_store = rustls::RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let tls_cfg = rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            let server_name = rustls::pki_types::ServerName::try_from(url.host.clone())
                .map_err(|_| format!("Invalid TLS host '{}'", url.host))?;
            let conn = rustls::ClientConnection::new(Arc::new(tls_cfg), server_name)
                .map_err(|e| format!("TLS init error: {}", e))?;
            let mut stream = rustls::StreamOwned::new(conn, tcp);
            stream
                .write_all(req.as_bytes())
                .map_err(|e| format!("TLS write header failed: {}", e))?;
            if !body.is_empty() {
                stream
                    .write_all(body)
                    .map_err(|e| format!("TLS write body failed: {}", e))?;
            }
            let _ = stream.flush();
            let _ = stream.read_to_end(&mut raw_resp);
        }
        #[cfg(not(feature = "tls-rustls"))]
        {
            return Err("HTTPS blob storage requires the 'tls-rustls' Cargo feature".to_string());
        }
    } else {
        tcp.write_all(req.as_bytes())
            .map_err(|e| format!("TCP write header failed: {}", e))?;
        if !body.is_empty() {
            tcp.write_all(body)
                .map_err(|e| format!("TCP write body failed: {}", e))?;
        }
        let _ = tcp.flush();
        let _ = tcp.read_to_end(&mut raw_resp);
    }

    let hdr_end = raw_resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "Malformed HTTP response from blob storage".to_string())?;
    let head = String::from_utf8_lossy(&raw_resp[..hdr_end]);
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let body_out = raw_resp[hdr_end + 4..].to_vec();
    Ok((status, body_out))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
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

fn base64_decode(input: &str) -> Option<Vec<u8>> {
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

/// Converts current UTC epoch into `(year, month, day, hour, min, sec, weekday)`.
fn utc_components() -> (i32, u32, u32, u32, u32, u32, u32) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let hour = (rem / 3600) as u32;
    let min = ((rem % 3600) / 60) as u32;
    let sec = (rem % 60) as u32;
    let weekday = ((days + 4).rem_euclid(7)) as u32; // 0 = Sun
                                                     // Howard Hinnant civil-from-days algorithm
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m, d, hour, min, sec, weekday)
}

fn utc_amz_date() -> String {
    let (y, m, d, h, min, s, _) = utc_components();
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}Z", y, m, d, h, min, s)
}

fn utc_rfc1123_date() -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, h, min, s, wd) = utc_components();
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[(wd as usize) % 7],
        d,
        MONTHS[((m.saturating_sub(1)) as usize) % 12],
        y,
        h,
        min,
        s
    )
}

fn base64url_encode(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

/// Builds a signed RS256 JWT assertion for Google Cloud Service Account OAuth2 token exchange.
#[cfg(feature = "tls-rustls")]
pub fn build_gcs_jwt_assertion(
    client_email: &str,
    private_key_pem: &str,
    token_uri: &str,
    now_secs: u64,
) -> Result<String, String> {
    use ring::rand::SystemRandom;
    use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

    let pem_body: String = private_key_pem
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("-----BEGIN") && !l.starts_with("-----END") && !l.is_empty())
        .collect();
    let pkcs8_der = base64_decode(&pem_body)
        .ok_or_else(|| "Invalid base64 in GCS service account private_key PEM".to_string())?;
    let key_pair = RsaKeyPair::from_pkcs8(&pkcs8_der)
        .map_err(|e| format!("Invalid PKCS#8 RSA private_key: {}", e))?;

    let header_json = r#"{"alg":"RS256","typ":"JWT"}"#;
    let claims_json = crate::json::JsonValue::Object(vec![
        (
            "iss".to_string(),
            crate::json::JsonValue::from(client_email),
        ),
        (
            "scope".to_string(),
            crate::json::JsonValue::from("https://www.googleapis.com/auth/devstorage.read_write"),
        ),
        ("aud".to_string(), crate::json::JsonValue::from(token_uri)),
        (
            "iat".to_string(),
            crate::json::JsonValue::from(now_secs as i64),
        ),
        (
            "exp".to_string(),
            crate::json::JsonValue::from((now_secs + 3600) as i64),
        ),
    ])
    .to_string_compact();

    let signing_input = format!(
        "{}.{}",
        base64url_encode(header_json.as_bytes()),
        base64url_encode(claims_json.as_bytes())
    );
    let rng = SystemRandom::new();
    let mut sig = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA256, &rng, signing_input.as_bytes(), &mut sig)
        .map_err(|e| format!("RSA-SHA256 JWT signing failed: {}", e))?;

    Ok(format!("{}.{}", signing_input, base64url_encode(&sig)))
}

#[cfg(feature = "tls-rustls")]
fn exchange_gcs_sa_key_file(sa_key_path: &str) -> Result<String, String> {
    let raw = fs::read_to_string(sa_key_path).map_err(|e| {
        format!(
            "Failed to read OMNI_GCS_SA_KEY_FILE '{}': {}",
            sa_key_path, e
        )
    })?;
    let parsed = crate::json::JsonValue::parse(&raw)
        .map_err(|e| format!("Invalid JSON in OMNI_GCS_SA_KEY_FILE: {}", e))?;
    let client_email = parsed
        .get("client_email")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Missing 'client_email' in GCS service account JSON".to_string())?;
    let private_key = parsed
        .get("private_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Missing 'private_key' in GCS service account JSON".to_string())?;
    let token_uri = parsed
        .get("token_uri")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("https://oauth2.googleapis.com/token");

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let jwt = build_gcs_jwt_assertion(client_email, private_key, token_uri, now_secs)?;
    let form_body = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}",
        jwt
    );
    let parsed_url = ParsedHttpUrl::parse(token_uri)?;
    let headers = vec![(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    )];
    let (status, resp_bytes) =
        http_blob_request(&parsed_url, "POST", &headers, form_body.as_bytes())?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "GCS OAuth2 token exchange failed with HTTP {}: {}",
            status,
            String::from_utf8_lossy(&resp_bytes)
        ));
    }
    let resp_str = String::from_utf8(resp_bytes)
        .map_err(|e| format!("Invalid UTF-8 from GCS OAuth2 token endpoint: {}", e))?;
    let resp_json = crate::json::JsonValue::parse(&resp_str)?;
    resp_json
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "Missing 'access_token' in GCS OAuth2 token response".to_string())
}

#[cfg(not(feature = "tls-rustls"))]
fn exchange_gcs_sa_key_file(_sa_key_path: &str) -> Result<String, String> {
    let _ = base64url_encode(&[]);
    Err("OMNI_GCS_SA_KEY_FILE requires the 'tls-rustls' feature".to_string())
}
