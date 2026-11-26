//! Isolated Experimental MCP File Uploads Extension (`io.modelcontextprotocol/files`, SEP-2356, v6.8.0).
//!
//! Feature-gated (`OMNI_ENABLE_FILE_UPLOADS_EXTENSION=false` by default) and intentionally isolated
//! here so any SEP-2356 draft wire adjustments before GA require zero changes to core gateway code.

use crate::json::JsonValue;
use crate::models::generate_ulid;
use crate::storage::FileBlobStore;

/// Maximum inline base64-decoded file size (256 KiB) allowed directly inside JSON-RPC arguments.
pub const INLINE_FILE_MAX_BYTES: usize = 256 * 1024;

/// Inspects a tool's `input_schema` and returns all property names annotated with `"x-mcp-file": true`
/// (or `"x-mcp-file": { ... }`).
pub fn find_mcp_file_properties(input_schema: &JsonValue) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(props) = input_schema.get("properties").and_then(|p| p.as_object()) {
        for (prop_name, prop_def) in props {
            if let Some(marker) = prop_def.get("x-mcp-file") {
                if !matches!(marker, JsonValue::Bool(false) | JsonValue::Null) {
                    out.push(prop_name.clone());
                }
            }
        }
    }
    out
}

/// Builds a SEP-2356 `resultType: "upload_required"` Multi-Round-Trip Response (MRTR)
/// with an HMAC-SHA256 signed upload URL (`PUT/POST /api/v1/files/upload/:upload_id?token=...`).
pub fn build_upload_required_mrtr(
    public_base_url: &str,
    field_name: &str,
    max_file_bytes: usize,
) -> JsonValue {
    let upload_id = format!("file_{}", generate_ulid());
    let token = FileBlobStore::sign_token("upload", &upload_id);
    let upload_url = format!(
        "{}/api/v1/files/upload/{}?token={}",
        public_base_url.trim_end_matches('/'),
        upload_id,
        token
    );
    json!({
        "resultType": "upload_required",
        "uploadId": upload_id,
        "field": field_name,
        "uploadUrl": upload_url,
        "method": "PUT",
        "maxBytes": max_file_bytes as i64,
        "inlineMaxBytes": INLINE_FILE_MAX_BYTES as i64
    })
}

/// Resolves inline file objects (`{"inlineBase64": "...", "filename": "...", "mimeType": "..."}`)
/// up to 256 KiB into `FileBlobStore` handles, or verifies existing `fileId` handles.
pub fn resolve_file_argument(
    store: &FileBlobStore,
    arg_val: &JsonValue,
    caller_actor: &str,
) -> Result<JsonValue, String> {
    if let Some(file_id) = arg_val
        .get("fileId")
        .or_else(|| arg_val.get("file_id"))
        .and_then(|v| v.as_str())
    {
        let meta = store
            .get_metadata(file_id)
            .ok_or_else(|| format!("Referenced fileId '{}' not found or expired", file_id))?;
        let dl_token = FileBlobStore::sign_token("download", &meta.file_id);
        return Ok(json!({
            "fileId": meta.file_id,
            "filename": meta.filename,
            "mimeType": meta.mime_type,
            "sizeBytes": meta.size_bytes as i64,
            "sha256": meta.sha256,
            "downloadToken": dl_token
        }));
    }

    if let Some(b64) = arg_val
        .get("inlineBase64")
        .or_else(|| arg_val.get("data"))
        .and_then(|v| v.as_str())
    {
        let bytes = decode_inline_base64(b64)
            .ok_or_else(|| "Invalid base64 in inline file payload".to_string())?;
        if bytes.len() > INLINE_FILE_MAX_BYTES {
            return Err(format!(
                "Inline file payload {} bytes exceeds 256 KiB limit; use uploadUrl streaming endpoint",
                bytes.len()
            ));
        }
        let filename = arg_val
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("inline.bin");
        let mime = arg_val
            .get("mimeType")
            .or_else(|| arg_val.get("mime_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream");
        let rec = store.put_file(None, filename, mime, caller_actor, &bytes)?;
        let dl_token = FileBlobStore::sign_token("download", &rec.file_id);
        return Ok(json!({
            "fileId": rec.file_id,
            "filename": rec.filename,
            "mimeType": rec.mime_type,
            "sizeBytes": rec.size_bytes as i64,
            "sha256": rec.sha256,
            "downloadToken": dl_token
        }));
    }

    Err("Expected either 'fileId' or 'inlineBase64' for x-mcp-file argument".to_string())
}

fn decode_inline_base64(input: &str) -> Option<Vec<u8>> {
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
