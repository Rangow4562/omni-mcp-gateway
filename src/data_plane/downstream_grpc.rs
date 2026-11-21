//! Live HTTP/2 gRPC & Connect-RPC Downstream Wire Client (v6.5.0).
//!
//! Pure safe Rust (`#![forbid(unsafe_code)]`) implementation supporting:
//! - **HTTP/2 gRPC** (`grpc://`, `grpcs://`, `h2c://`): RFC 7540 connection preface, SETTINGS
//!   handshake, chunked DATA framing (`<= 16384` bytes per frame), flow-control `WINDOW_UPDATE`,
//!   `CONTINUATION` header assembly, full RFC 7541 HPACK decoder (static table + dynamic table +
//!   Appendix B Huffman decoding), 5-byte length-prefixed gRPC DATA framing, trailers
//!   (`grpc-status`, `grpc-message`), and TLS ALPN `h2`.
//! - **Connect-RPC** (`http://`, `https://`): `Connect-Protocol-Version: 1` unary JSON or
//!   5-byte framed `application/connect+json` over HTTP/1.1 & HTTPS.
//!
//! Errors propagate directly into `CircuitBreakerRegistry` and `MutationCompensationSpec` Sagas.

use crate::data_plane::downstream_http::HttpDownstreamClient;
use crate::json::JsonValue;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const H2_CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const H2_DEFAULT_MAX_FRAME_SIZE: usize = 16_384;

// RFC 7540 frame types
const FRAME_DATA: u8 = 0x00;
const FRAME_HEADERS: u8 = 0x01;
const FRAME_RST_STREAM: u8 = 0x03;
const FRAME_SETTINGS: u8 = 0x04;
const FRAME_PING: u8 = 0x06;
const FRAME_GOAWAY: u8 = 0x07;
const FRAME_WINDOW_UPDATE: u8 = 0x08;
const FRAME_CONTINUATION: u8 = 0x09;

// Frame flags
const FLAG_ACK: u8 = 0x01;
const FLAG_END_STREAM: u8 = 0x01;
const FLAG_END_HEADERS: u8 = 0x04;
const FLAG_PADDED: u8 = 0x08;
const FLAG_PRIORITY: u8 = 0x20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrpcWireMode {
    /// RFC 7540 HTTP/2 gRPC (`h2c` or TLS `h2` ALPN) with 5-byte length-prefixed DATA framing.
    Http2Grpc { use_tls: bool },
    /// Connect-RPC (`Connect-Protocol-Version: 1`) over HTTP/1.1 or HTTPS.
    ConnectJson,
}

#[derive(Clone)]
pub struct GrpcDownstreamClient {
    http_client: HttpDownstreamClient,
    connect_timeout: Duration,
    read_timeout: Duration,
}

impl Default for GrpcDownstreamClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GrpcDownstreamClient {
    pub fn new() -> Self {
        Self {
            http_client: HttpDownstreamClient::new(),
            connect_timeout: Duration::from_secs(3),
            read_timeout: Duration::from_secs(10),
        }
    }

    /// Parses a gRPC or Connect endpoint URL into `(mode, host, port, path)`.
    pub fn parse_endpoint(
        endpoint: &str,
        service_method_fallback: &str,
    ) -> Result<(GrpcWireMode, String, u16, String), String> {
        let trimmed = endpoint.trim();
        let (mode, default_port, rest) = if let Some(s) = trimmed.strip_prefix("grpc://") {
            (GrpcWireMode::Http2Grpc { use_tls: false }, 50051u16, s)
        } else if let Some(s) = trimmed.strip_prefix("h2c://") {
            (GrpcWireMode::Http2Grpc { use_tls: false }, 50051u16, s)
        } else if let Some(s) = trimmed.strip_prefix("grpcs://") {
            (GrpcWireMode::Http2Grpc { use_tls: true }, 443u16, s)
        } else if let Some(s) = trimmed.strip_prefix("http://") {
            (GrpcWireMode::ConnectJson, 80u16, s)
        } else if let Some(s) = trimmed.strip_prefix("https://") {
            (GrpcWireMode::ConnectJson, 443u16, s)
        } else {
            return Err(format!(
                "Unsupported gRPC endpoint scheme '{}'. Expected grpc://, grpcs://, h2c://, http://, or https://",
                endpoint
            ));
        };

        let (host_port, raw_path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };

        if host_port.is_empty() {
            return Err(format!("Missing host in gRPC endpoint '{}'", endpoint));
        }

        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) if !h.contains(']') || host_port.starts_with('[') => {
                let port_num = p
                    .parse::<u16>()
                    .map_err(|_| format!("Invalid gRPC port '{}'", p))?;
                (h.trim_matches(&['[', ']'][..]).to_string(), port_num)
            }
            _ => (
                host_port.trim_matches(&['[', ']'][..]).to_string(),
                default_port,
            ),
        };

        if host.is_empty() {
            return Err(format!("Missing host in gRPC endpoint '{}'", endpoint));
        }

        let path = if raw_path.is_empty() || raw_path == "/" {
            let clean = service_method_fallback.trim_start_matches('/');
            format!("/{}", clean)
        } else {
            raw_path.to_string()
        };

        Ok((mode, host, port, path))
    }

    /// Encodes a 5-byte gRPC / Connect length-prefixed envelope: `[0x00][u32 BE len][payload]`.
    pub fn encode_grpc_frame(payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(0x00); // uncompressed
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    /// Decodes a 5-byte gRPC / Connect length-prefixed envelope, returning the message payload bytes.
    pub fn decode_grpc_frame(framed: &[u8]) -> Result<Vec<u8>, String> {
        if framed.len() < 5 {
            return Err(format!(
                "Truncated gRPC frame ({} bytes, expected >= 5)",
                framed.len()
            ));
        }
        let flags = framed[0];
        if flags == 1 {
            return Err("Compressed gRPC frames (flag=1) are not supported".to_string());
        }
        if flags != 0 && flags != 2 {
            return Err(format!("Unsupported gRPC frame flags byte: {}", flags));
        }
        let len = u32::from_be_bytes([framed[1], framed[2], framed[3], framed[4]]) as usize;
        if framed.len() < 5 + len {
            return Err(format!(
                "Incomplete gRPC frame payload: expected {} bytes, got {}",
                len,
                framed.len() - 5
            ));
        }
        Ok(framed[5..5 + len].to_vec())
    }

    /// Invokes a live downstream gRPC or Connect-RPC service.
    pub fn invoke(
        &self,
        endpoint: &str,
        service_method: &str,
        arguments: &JsonValue,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Vec<u8>, String> {
        let trimmed = endpoint.trim();
        let (mode, host, port, path) = Self::parse_endpoint(trimmed, service_method)?;
        let json_payload = arguments.to_string_compact().into_bytes();

        match mode {
            GrpcWireMode::ConnectJson => {
                let mut hdrs = extra_headers.cloned().unwrap_or_default();
                let use_framed_connect = hdrs.iter().any(|(k, v)| {
                    k.eq_ignore_ascii_case("content-type")
                        && v.eq_ignore_ascii_case("application/connect+json")
                });
                // Deduplicate any pre-existing Content-Type or Connect-Protocol-Version headers
                hdrs.retain(|k, _| {
                    !k.eq_ignore_ascii_case("content-type")
                        && !k.eq_ignore_ascii_case("connect-protocol-version")
                });
                hdrs.insert("Connect-Protocol-Version".to_string(), "1".to_string());

                let scheme = if trimmed.starts_with("https://") {
                    "https"
                } else {
                    "http"
                };
                let host_fmt = if host.contains(':') {
                    format!("[{}]", host)
                } else {
                    host.clone()
                };
                let url = format!("{}://{}:{}{}", scheme, host_fmt, port, path);

                if use_framed_connect {
                    hdrs.insert(
                        "Content-Type".to_string(),
                        "application/connect+json".to_string(),
                    );
                    let framed = Self::encode_grpc_frame(&json_payload);
                    let resp = self.http_client.post_json(&url, &framed, Some(&hdrs))?;
                    Self::decode_grpc_frame(&resp)
                } else {
                    hdrs.insert("Content-Type".to_string(), "application/json".to_string());
                    self.http_client.post_json(&url, &json_payload, Some(&hdrs))
                }
            }
            GrpcWireMode::Http2Grpc { use_tls } => {
                self.invoke_h2_grpc(use_tls, &host, port, &path, &json_payload, extra_headers)
            }
        }
    }

    fn invoke_h2_grpc(
        &self,
        use_tls: bool,
        host: &str,
        port: u16,
        path: &str,
        payload: &[u8],
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Vec<u8>, String> {
        let addr_key = if host.contains(':') {
            format!("[{}]:{}", host, port)
        } else {
            format!("{}:{}", host, port)
        };
        let addrs: Vec<_> = addr_key
            .to_socket_addrs()
            .map_err(|e| format!("gRPC DNS resolution failed for '{}': {}", addr_key, e))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("No socket addresses resolved for '{}'", addr_key));
        }

        let tcp = TcpStream::connect_timeout(&addrs[0], self.connect_timeout)
            .map_err(|e| format!("gRPC TCP connect to '{}' failed: {}", addr_key, e))?;
        let _ = tcp.set_read_timeout(Some(self.read_timeout));
        let _ = tcp.set_write_timeout(Some(self.connect_timeout));
        let _ = tcp.set_nodelay(true);

        if use_tls {
            #[cfg(not(feature = "tls-rustls"))]
            {
                let _ = (path, payload, extra_headers);
                return Err(
                    "gRPC over TLS (grpcs://) requires the 'tls-rustls' feature".to_string()
                );
            }
            #[cfg(feature = "tls-rustls")]
            {
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let mut cfg = rustls::ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                cfg.alpn_protocols = vec![b"h2".to_vec()];
                let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
                    .map_err(|e| format!("Invalid gRPC TLS SNI '{}': {}", host, e))?;
                let conn = rustls::ClientConnection::new(std::sync::Arc::new(cfg), server_name)
                    .map_err(|e| format!("gRPC TLS init failed: {}", e))?;
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                return exchange_h2_stream(
                    &mut tls,
                    "https",
                    &addr_key,
                    path,
                    payload,
                    extra_headers,
                );
            }
        }

        let mut stream = tcp;
        exchange_h2_stream(&mut stream, "http", &addr_key, path, payload, extra_headers)
    }
}

fn exchange_h2_stream<S: Read + Write>(
    stream: &mut S,
    scheme: &str,
    authority: &str,
    path: &str,
    payload: &[u8],
    extra_headers: Option<&HashMap<String, String>>,
) -> Result<Vec<u8>, String> {
    // 1. Send HTTP/2 client preface + empty client SETTINGS
    stream
        .write_all(H2_CLIENT_PREFACE)
        .map_err(|e| format!("Failed writing H2 preface: {}", e))?;
    write_h2_frame(stream, FRAME_SETTINGS, 0, 0, &[])?;

    // 2. Encode HPACK literal header block for stream 1
    let mut header_block = Vec::new();
    encode_hpack_literal(&mut header_block, ":method", "POST");
    encode_hpack_literal(&mut header_block, ":scheme", scheme);
    encode_hpack_literal(&mut header_block, ":path", path);
    encode_hpack_literal(&mut header_block, ":authority", authority);
    let content_type = extra_headers
        .and_then(|h| {
            h.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                .map(|(_, v)| v.as_str())
        })
        .unwrap_or("application/grpc+json");
    encode_hpack_literal(&mut header_block, "content-type", content_type);
    encode_hpack_literal(&mut header_block, "te", "trailers");
    encode_hpack_literal(&mut header_block, "user-agent", "omni-mcp-h2/6.5.0");
    if let Some(hdrs) = extra_headers {
        for (k, v) in hdrs {
            let kl = k.to_ascii_lowercase();
            if !matches!(
                kl.as_str(),
                "content-type" | "host" | "connection" | "transfer-encoding" | "te"
            ) {
                encode_hpack_literal(&mut header_block, &kl, v);
            }
        }
    }

    // Fragment HEADERS if exceeding default max frame size
    if header_block.len() <= H2_DEFAULT_MAX_FRAME_SIZE {
        write_h2_frame(stream, FRAME_HEADERS, FLAG_END_HEADERS, 1, &header_block)?;
    } else {
        let chunks: Vec<&[u8]> = header_block.chunks(H2_DEFAULT_MAX_FRAME_SIZE).collect();
        for (idx, chunk) in chunks.iter().enumerate() {
            let is_last = idx + 1 == chunks.len();
            let ftype = if idx == 0 {
                FRAME_HEADERS
            } else {
                FRAME_CONTINUATION
            };
            let flags = if is_last { FLAG_END_HEADERS } else { 0 };
            write_h2_frame(stream, ftype, flags, 1, chunk)?;
        }
    }

    // 3. Send 5-byte gRPC-framed DATA chunked to <= 16384 bytes (RFC 7540 DEFAULT_MAX_FRAME_SIZE)
    let framed_body = GrpcDownstreamClient::encode_grpc_frame(payload);
    let data_chunks: Vec<&[u8]> = framed_body.chunks(H2_DEFAULT_MAX_FRAME_SIZE).collect();
    for (idx, chunk) in data_chunks.iter().enumerate() {
        let flags = if idx + 1 == data_chunks.len() {
            FLAG_END_STREAM
        } else {
            0
        };
        write_h2_frame(stream, FRAME_DATA, flags, 1, chunk)?;
    }
    stream
        .flush()
        .map_err(|e| format!("Failed flushing H2 request: {}", e))?;

    // 4. Read server frames until stream 1 closes (END_STREAM)
    let mut response_data = Vec::new();
    let mut response_headers: HashMap<String, String> = HashMap::new();
    let mut hpack_decoder = HpackDecoder::new(4096);
    let mut pending_header_block: Option<(Vec<u8>, bool)> = None;
    let mut stream_closed = false;

    while !stream_closed {
        let (ftype, flags, stream_id, body) = read_h2_frame(stream)?;
        match ftype {
            FRAME_SETTINGS => {
                if (flags & FLAG_ACK) == 0 {
                    write_h2_frame(stream, FRAME_SETTINGS, FLAG_ACK, 0, &[])?;
                    let _ = stream.flush();
                }
            }
            FRAME_PING => {
                if (flags & FLAG_ACK) == 0 && body.len() == 8 {
                    write_h2_frame(stream, FRAME_PING, FLAG_ACK, 0, &body)?;
                    let _ = stream.flush();
                }
            }
            FRAME_WINDOW_UPDATE => {}
            FRAME_HEADERS if stream_id == 1 => {
                let block = strip_headers_padding_priority(&body, flags);
                let end_stream = (flags & FLAG_END_STREAM) != 0;
                if (flags & FLAG_END_HEADERS) != 0 {
                    hpack_decoder.decode_block(block, &mut response_headers);
                    if end_stream {
                        stream_closed = true;
                    }
                } else {
                    pending_header_block = Some((block.to_vec(), end_stream));
                }
            }
            FRAME_CONTINUATION if stream_id == 1 => {
                if let Some((mut buf, end_stream)) = pending_header_block.take() {
                    buf.extend_from_slice(&body);
                    if (flags & FLAG_END_HEADERS) != 0 {
                        hpack_decoder.decode_block(&buf, &mut response_headers);
                        if end_stream {
                            stream_closed = true;
                        }
                    } else {
                        pending_header_block = Some((buf, end_stream));
                    }
                }
            }
            FRAME_DATA if stream_id == 1 => {
                let raw_frame_len = body.len() as u32;
                let data_slice = if (flags & FLAG_PADDED) != 0 && !body.is_empty() {
                    let pad = body[0] as usize;
                    let end = body.len().saturating_sub(pad);
                    if 1 <= end {
                        &body[1..end]
                    } else {
                        &[]
                    }
                } else {
                    &body
                };
                response_data.extend_from_slice(data_slice);
                // Replenish HTTP/2 flow-control windows (connection + stream) so >64 KiB responses never stall
                if raw_frame_len > 0 && (flags & FLAG_END_STREAM) == 0 {
                    let inc = (raw_frame_len & 0x7FFF_FFFF).to_be_bytes();
                    let _ = write_h2_frame(stream, FRAME_WINDOW_UPDATE, 0, 0, &inc);
                    let _ = write_h2_frame(stream, FRAME_WINDOW_UPDATE, 0, 1, &inc);
                    let _ = stream.flush();
                }
                if (flags & FLAG_END_STREAM) != 0 {
                    stream_closed = true;
                }
            }
            FRAME_RST_STREAM if stream_id == 1 => {
                let err_code = if body.len() >= 4 {
                    u32::from_be_bytes([body[0], body[1], body[2], body[3]])
                } else {
                    0
                };
                return Err(format!("gRPC H2 stream reset with error code {}", err_code));
            }
            FRAME_GOAWAY => {
                if !response_data.is_empty() {
                    break;
                }
                return Err("gRPC peer closed HTTP/2 connection with GOAWAY".to_string());
            }
            _ => {}
        }
    }

    if let Some(grpc_status) = response_headers.get("grpc-status") {
        if grpc_status != "0" {
            let msg = response_headers
                .get("grpc-message")
                .cloned()
                .unwrap_or_else(|| "unknown gRPC error".to_string());
            return Err(format!("gRPC status {}: {}", grpc_status, msg));
        }
    }

    if let Some(http_status) = response_headers.get(":status") {
        if !http_status.starts_with('2') {
            return Err(format!("gRPC HTTP/2 status {}", http_status));
        }
    }

    if response_data.len() >= 5 {
        GrpcDownstreamClient::decode_grpc_frame(&response_data)
    } else {
        Ok(response_data)
    }
}

pub fn write_h2_frame<W: Write>(
    w: &mut W,
    frame_type: u8,
    flags: u8,
    stream_id: u32,
    payload: &[u8],
) -> Result<(), String> {
    let len = payload.len();
    let mut hdr = [0u8; 9];
    hdr[0] = ((len >> 16) & 0xFF) as u8;
    hdr[1] = ((len >> 8) & 0xFF) as u8;
    hdr[2] = (len & 0xFF) as u8;
    hdr[3] = frame_type;
    hdr[4] = flags;
    let sid = stream_id & 0x7FFF_FFFF;
    hdr[5..9].copy_from_slice(&sid.to_be_bytes());
    w.write_all(&hdr)
        .map_err(|e| format!("Failed writing H2 frame header: {}", e))?;
    if !payload.is_empty() {
        w.write_all(payload)
            .map_err(|e| format!("Failed writing H2 frame body: {}", e))?;
    }
    Ok(())
}

pub fn read_h2_frame<R: Read>(r: &mut R) -> Result<(u8, u8, u32, Vec<u8>), String> {
    let mut hdr = [0u8; 9];
    r.read_exact(&mut hdr)
        .map_err(|e| format!("Failed reading H2 frame header: {}", e))?;
    let len = ((hdr[0] as usize) << 16) | ((hdr[1] as usize) << 8) | (hdr[2] as usize);
    let ftype = hdr[3];
    let flags = hdr[4];
    let stream_id = u32::from_be_bytes([hdr[5], hdr[6], hdr[7], hdr[8]]) & 0x7FFF_FFFF;
    if len > 16 * 1024 * 1024 {
        return Err(format!("H2 frame length {} exceeds 16MB cap", len));
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        r.read_exact(&mut body)
            .map_err(|e| format!("Failed reading H2 frame payload: {}", e))?;
    }
    Ok((ftype, flags, stream_id, body))
}

fn strip_headers_padding_priority(body: &[u8], flags: u8) -> &[u8] {
    let mut start = 0usize;
    let mut end = body.len();
    if (flags & FLAG_PADDED) != 0 && !body.is_empty() {
        let pad = body[0] as usize;
        start = 1;
        end = end.saturating_sub(pad);
    }
    if (flags & FLAG_PRIORITY) != 0 {
        start = (start + 5).min(end);
    }
    if start <= end {
        &body[start..end]
    } else {
        &[]
    }
}

/// Encodes a literal header field without indexing (RFC 7541 §6.2.2, prefix `0x00`).
pub fn encode_hpack_literal(out: &mut Vec<u8>, name: &str, value: &str) {
    out.push(0x00);
    encode_hpack_str(out, name.as_bytes());
    encode_hpack_str(out, value.as_bytes());
}

fn encode_hpack_str(out: &mut Vec<u8>, bytes: &[u8]) {
    encode_hpack_int(out, bytes.len(), 7, 0x00);
    out.extend_from_slice(bytes);
}

fn encode_hpack_int(out: &mut Vec<u8>, mut value: usize, prefix_bits: u8, mask: u8) {
    let max_prefix = (1usize << prefix_bits) - 1;
    if value < max_prefix {
        out.push(mask | (value as u8));
    } else {
        out.push(mask | (max_prefix as u8));
        value -= max_prefix;
        while value >= 128 {
            out.push(((value & 0x7F) as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }
}

fn decode_hpack_int(buf: &[u8], pos: &mut usize, prefix_bits: u8) -> Option<usize> {
    if *pos >= buf.len() {
        return None;
    }
    let max_prefix = (1usize << prefix_bits) - 1;
    let mut val = (buf[*pos] as usize) & max_prefix;
    *pos += 1;
    if val < max_prefix {
        return Some(val);
    }
    let mut m = 0u32;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        val = val.checked_add(((b & 0x7F) as usize) << m)?;
        if (b & 0x80) == 0 {
            return Some(val);
        }
        m += 7;
        if m > 28 {
            return None;
        }
    }
    None
}

fn decode_hpack_str(buf: &[u8], pos: &mut usize) -> Option<String> {
    if *pos >= buf.len() {
        return None;
    }
    let huffman = (buf[*pos] & 0x80) != 0;
    let len = decode_hpack_int(buf, pos, 7)?;
    if *pos + len > buf.len() {
        return None;
    }
    let slice = &buf[*pos..*pos + len];
    *pos += len;
    if huffman {
        let decoded = hpack_huffman_decode(slice)?;
        Some(String::from_utf8_lossy(&decoded).into_owned())
    } else {
        Some(String::from_utf8_lossy(slice).into_owned())
    }
}

/// Full RFC 7541 Appendix A Static Table (indices 1..=61).
const HPACK_STATIC_TABLE: &[(&str, &str)] = &[
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// Stateful RFC 7541 HPACK Decoder supporting static table, FIFO dynamic table, and Huffman decoding.
pub struct HpackDecoder {
    dynamic_table: VecDeque<(String, String)>,
    dynamic_size: usize,
    max_dynamic_size: usize,
}

impl HpackDecoder {
    pub fn new(max_dynamic_size: usize) -> Self {
        Self {
            dynamic_table: VecDeque::new(),
            dynamic_size: 0,
            max_dynamic_size,
        }
    }

    fn get_indexed(&self, idx: usize) -> Option<(String, String)> {
        if idx == 0 {
            return None;
        }
        if idx <= HPACK_STATIC_TABLE.len() {
            let (k, v) = HPACK_STATIC_TABLE[idx - 1];
            return Some((k.to_string(), v.to_string()));
        }
        let dyn_idx = idx - HPACK_STATIC_TABLE.len() - 1;
        self.dynamic_table.get(dyn_idx).cloned()
    }

    fn insert_dynamic(&mut self, name: String, value: String) {
        let entry_size = name.len() + value.len() + 32;
        if entry_size > self.max_dynamic_size {
            self.dynamic_table.clear();
            self.dynamic_size = 0;
            return;
        }
        while self.dynamic_size + entry_size > self.max_dynamic_size {
            if let Some((old_k, old_v)) = self.dynamic_table.pop_back() {
                self.dynamic_size = self
                    .dynamic_size
                    .saturating_sub(old_k.len() + old_v.len() + 32);
            } else {
                break;
            }
        }
        self.dynamic_table.push_front((name, value));
        self.dynamic_size += entry_size;
    }

    fn update_max_size(&mut self, new_max: usize) {
        self.max_dynamic_size = new_max;
        while self.dynamic_size > self.max_dynamic_size {
            if let Some((old_k, old_v)) = self.dynamic_table.pop_back() {
                self.dynamic_size = self
                    .dynamic_size
                    .saturating_sub(old_k.len() + old_v.len() + 32);
            } else {
                break;
            }
        }
    }

    pub fn decode_block(&mut self, buf: &[u8], out: &mut HashMap<String, String>) {
        let mut pos = 0usize;
        while pos < buf.len() {
            let b = buf[pos];
            if (b & 0x80) != 0 {
                // Indexed Header Field
                if let Some(idx) = decode_hpack_int(buf, &mut pos, 7) {
                    if let Some((k, v)) = self.get_indexed(idx) {
                        out.insert(k.to_ascii_lowercase(), v);
                    }
                } else {
                    break;
                }
            } else if (b & 0x40) != 0 {
                // Literal Header Field with Incremental Indexing
                let Some(name_idx) = decode_hpack_int(buf, &mut pos, 6) else {
                    break;
                };
                let name = if name_idx == 0 {
                    let Some(s) = decode_hpack_str(buf, &mut pos) else {
                        break;
                    };
                    s
                } else {
                    self.get_indexed(name_idx)
                        .map(|(k, _)| k)
                        .unwrap_or_default()
                };
                let Some(val) = decode_hpack_str(buf, &mut pos) else {
                    break;
                };
                if !name.is_empty() {
                    let lc = name.to_ascii_lowercase();
                    self.insert_dynamic(lc.clone(), val.clone());
                    out.insert(lc, val);
                }
            } else if (b & 0x20) != 0 {
                // Dynamic Table Size Update
                let Some(new_max) = decode_hpack_int(buf, &mut pos, 5) else {
                    break;
                };
                self.update_max_size(new_max);
            } else {
                // Literal Header Field without Indexing / Never Indexed
                let Some(name_idx) = decode_hpack_int(buf, &mut pos, 4) else {
                    break;
                };
                let name = if name_idx == 0 {
                    let Some(s) = decode_hpack_str(buf, &mut pos) else {
                        break;
                    };
                    s
                } else {
                    self.get_indexed(name_idx)
                        .map(|(k, _)| k)
                        .unwrap_or_default()
                };
                let Some(val) = decode_hpack_str(buf, &mut pos) else {
                    break;
                };
                if !name.is_empty() {
                    out.insert(name.to_ascii_lowercase(), val);
                }
            }
        }
    }
}

/// Stateless helper wrapper decoding a single HPACK header block.
pub fn decode_hpack_headers(buf: &[u8], out: &mut HashMap<String, String>) {
    let mut dec = HpackDecoder::new(4096);
    dec.decode_block(buf, out);
}

/// RFC 7541 Appendix B Huffman codes `(code, bit_len)` for byte symbols `0..=255`.
const HPACK_HUFFMAN_CODES: [(u32, u8); 256] = [
    (0x1ff8, 13),
    (0x7fffd8, 23),
    (0xfffffe2, 28),
    (0xfffffe3, 28),
    (0xfffffe4, 28),
    (0xfffffe5, 28),
    (0xfffffe6, 28),
    (0xfffffe7, 28),
    (0xfffffe8, 28),
    (0xffffea, 24),
    (0x3ffffffc, 30),
    (0xfffffe9, 28),
    (0xfffffea, 28),
    (0x3ffffffd, 30),
    (0xfffffeb, 28),
    (0xfffffec, 28),
    (0xfffffed, 28),
    (0xfffffee, 28),
    (0xfffffef, 28),
    (0xffffff0, 28),
    (0xffffff1, 28),
    (0xffffff2, 28),
    (0x3ffffffe, 30),
    (0xffffff3, 28),
    (0xffffff4, 28),
    (0xffffff5, 28),
    (0xffffff6, 28),
    (0xffffff7, 28),
    (0xffffff8, 28),
    (0xffffff9, 28),
    (0xffffffa, 28),
    (0xffffffb, 28),
    (0x14, 6),
    (0x3f8, 10),
    (0x3f9, 10),
    (0xffa, 12),
    (0x1ff9, 13),
    (0x15, 6),
    (0xf8, 8),
    (0x7fa, 11),
    (0x3fa, 10),
    (0x3fb, 10),
    (0xf9, 8),
    (0x7fb, 11),
    (0xfa, 8),
    (0x16, 6),
    (0x17, 6),
    (0x18, 6),
    (0x0, 5),
    (0x1, 5),
    (0x2, 5),
    (0x19, 6),
    (0x1a, 6),
    (0x1b, 6),
    (0x1c, 6),
    (0x1d, 6),
    (0x1e, 6),
    (0x1f, 6),
    (0x5c, 7),
    (0xfb, 8),
    (0x7ffc, 15),
    (0x20, 6),
    (0xffb, 12),
    (0x3fc, 10),
    (0x1ffa, 13),
    (0x21, 6),
    (0x5d, 7),
    (0x5e, 7),
    (0x5f, 7),
    (0x60, 7),
    (0x61, 7),
    (0x62, 7),
    (0x63, 7),
    (0x64, 7),
    (0x65, 7),
    (0x66, 7),
    (0x67, 7),
    (0x68, 7),
    (0x69, 7),
    (0x6a, 7),
    (0x6b, 7),
    (0x6c, 7),
    (0x6d, 7),
    (0x6e, 7),
    (0x6f, 7),
    (0x70, 7),
    (0x71, 7),
    (0x72, 7),
    (0xfc, 8),
    (0x73, 7),
    (0xfd, 8),
    (0x1ffb, 13),
    (0x7fff0, 19),
    (0x1ffc, 13),
    (0x3ffc, 14),
    (0x22, 6),
    (0x7ffd, 15),
    (0x3, 5),
    (0x23, 6),
    (0x4, 5),
    (0x24, 6),
    (0x5, 5),
    (0x25, 6),
    (0x26, 6),
    (0x27, 6),
    (0x6, 5),
    (0x74, 7),
    (0x75, 7),
    (0x28, 6),
    (0x29, 6),
    (0x2a, 6),
    (0x7, 5),
    (0x2b, 6),
    (0x76, 7),
    (0x2c, 6),
    (0x8, 5),
    (0x9, 5),
    (0x2d, 6),
    (0x77, 7),
    (0x78, 7),
    (0x79, 7),
    (0x7a, 7),
    (0x7b, 7),
    (0x7ffe, 15),
    (0x7fc, 11),
    (0x3ffd, 14),
    (0x1ffd, 13),
    (0xffffffc, 28),
    (0xfffe6, 20),
    (0x3fffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x3fffd3, 22),
    (0x3fffd4, 22),
    (0x3fffd5, 22),
    (0x7fffd9, 23),
    (0x3fffd6, 22),
    (0x7fffda, 23),
    (0x7fffdb, 23),
    (0x7fffdc, 23),
    (0x7fffdd, 23),
    (0x7fffde, 23),
    (0xffffeb, 24),
    (0x7fffdf, 23),
    (0xffffec, 24),
    (0xffffed, 24),
    (0x3fffd7, 22),
    (0x7fffe0, 23),
    (0xffffee, 24),
    (0x7fffe1, 23),
    (0x7fffe2, 23),
    (0x7fffe3, 23),
    (0x7fffe4, 23),
    (0x1fffdc, 21),
    (0x3fffd8, 22),
    (0x7fffe5, 23),
    (0x3fffd9, 22),
    (0x7fffe6, 23),
    (0x7fffe7, 23),
    (0xffffef, 24),
    (0x3fffda, 22),
    (0x1fffdd, 21),
    (0xfffe9, 20),
    (0x3fffdb, 22),
    (0x3fffdc, 22),
    (0x7fffe8, 23),
    (0x7fffe9, 23),
    (0x1fffde, 21),
    (0x7fffea, 23),
    (0x3fffdd, 22),
    (0x3fffde, 22),
    (0xfffff0, 24),
    (0x1fffdf, 21),
    (0x3fffdf, 22),
    (0x7fffeb, 23),
    (0x7fffec, 23),
    (0x1fffe0, 21),
    (0x1fffe1, 21),
    (0x3fffe0, 22),
    (0x1fffe2, 21),
    (0x7fffed, 23),
    (0x3fffe1, 22),
    (0x7fffee, 23),
    (0x7fffef, 23),
    (0xfffea, 20),
    (0x3fffe2, 22),
    (0x3fffe3, 22),
    (0x3fffe4, 22),
    (0x7ffff0, 23),
    (0x3fffe5, 22),
    (0x3fffe6, 22),
    (0x7ffff1, 23),
    (0x3ffffe0, 26),
    (0x3ffffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x3fffe7, 22),
    (0x7ffff2, 23),
    (0x3fffe8, 22),
    (0x1ffffec, 25),
    (0x3ffffe2, 26),
    (0x3ffffe3, 26),
    (0x3ffffe4, 26),
    (0x7ffffde, 27),
    (0x7ffffdf, 27),
    (0x3ffffe5, 26),
    (0xfffff1, 24),
    (0x1ffffed, 25),
    (0x7fff2, 19),
    (0x1fffe3, 21),
    (0x3ffffe6, 26),
    (0x7ffffe0, 27),
    (0x7ffffe1, 27),
    (0x3ffffe7, 26),
    (0x7ffffe2, 27),
    (0xfffff2, 24),
    (0x1fffe4, 21),
    (0x1fffe5, 21),
    (0x3ffffe8, 26),
    (0x3ffffe9, 26),
    (0xffffffd, 28),
    (0x7ffffe3, 27),
    (0x7ffffe4, 27),
    (0x7ffffe5, 27),
    (0xfffec, 20),
    (0xfffff3, 24),
    (0xfffed, 20),
    (0x1fffe6, 21),
    (0x3fffe9, 22),
    (0x1fffe7, 21),
    (0x1fffe8, 21),
    (0x7ffff3, 23),
    (0x3fffea, 22),
    (0x3fffeb, 22),
    (0x1ffffee, 25),
    (0x1ffffef, 25),
    (0xfffff4, 24),
    (0xfffff5, 24),
    (0x3ffffea, 26),
    (0x7ffff4, 23),
    (0x3ffffeb, 26),
    (0x7ffffe6, 27),
    (0x3ffffec, 26),
    (0x3ffffed, 26),
    (0x7ffffe7, 27),
    (0x7ffffe8, 27),
    (0x7ffffe9, 27),
    (0x7ffffea, 27),
    (0x7ffffeb, 27),
    (0xffffffe, 28),
    (0x7ffffec, 27),
    (0x7ffffed, 27),
    (0x7ffffee, 27),
    (0x7ffffef, 27),
    (0x7fffff0, 27),
    (0x3ffffee, 26),
];

#[derive(Clone, Default)]
struct HuffmanNode {
    children: [Option<usize>; 2],
    symbol: Option<u8>,
}

fn huffman_trie() -> &'static [HuffmanNode] {
    static TRIE: std::sync::OnceLock<Vec<HuffmanNode>> = std::sync::OnceLock::new();
    TRIE.get_or_init(|| {
        let mut nodes = vec![HuffmanNode::default()];
        for (sym, &(code, len)) in HPACK_HUFFMAN_CODES.iter().enumerate() {
            let mut cur = 0usize;
            for shift in (0..len).rev() {
                let bit = ((code >> shift) & 1) as usize;
                let next = match nodes[cur].children[bit] {
                    Some(idx) => idx,
                    None => {
                        let idx = nodes.len();
                        nodes.push(HuffmanNode::default());
                        nodes[cur].children[bit] = Some(idx);
                        idx
                    }
                };
                cur = next;
            }
            nodes[cur].symbol = Some(sym as u8);
        }
        nodes
    })
}

/// Decodes an RFC 7541 Appendix B Huffman-encoded byte slice.
pub fn hpack_huffman_decode(encoded: &[u8]) -> Option<Vec<u8>> {
    let trie = huffman_trie();
    let mut out = Vec::with_capacity(encoded.len() * 2);
    let mut node = 0usize;
    let mut pending_bits = 0u8;
    let mut pending_all_ones = true;

    for &byte in encoded {
        for shift in (0..8).rev() {
            let bit = ((byte >> shift) & 1) as usize;
            if bit == 0 {
                pending_all_ones = false;
            }
            pending_bits = pending_bits.saturating_add(1);
            let Some(next) = trie[node].children[bit] else {
                // EOS prefix or invalid Huffman code
                return None;
            };
            node = next;
            if let Some(sym) = trie[node].symbol {
                out.push(sym);
                node = 0;
                pending_bits = 0;
                pending_all_ones = true;
            }
        }
    }

    // RFC 7541 §5.2: trailing padding must be <= 7 bits and all 1s
    if pending_bits > 7 || (pending_bits > 0 && !pending_all_ones) {
        return None;
    }
    Some(out)
}

/// Encodes raw bytes using RFC 7541 Appendix B Huffman coding (useful for tests and wire interop).
pub fn hpack_huffman_encode(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut acc: u64 = 0;
    let mut bits: u8 = 0;
    for &b in raw {
        let (code, len) = HPACK_HUFFMAN_CODES[b as usize];
        acc = (acc << len) | (code as u64);
        bits += len;
        while bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
    }
    if bits > 0 {
        let pad = 8 - bits;
        let last = (((acc << pad) | ((1u64 << pad) - 1)) & 0xFF) as u8;
        out.push(last);
    }
    out
}
