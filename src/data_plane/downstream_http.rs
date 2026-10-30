//! Live HTTP/1.1 Downstream Transport (`transport-http`).
//! Zero-dependency, `std`-only HTTP/1.1 client over raw `TcpStream`.
//! Features:
//! - Connect, read, and write timeouts
//! - Keep-alive connection pooling
//! - Size caps (max 10MB payload)
//! - Bounded retries with backoff
//! - Feeds errors to circuit breaker (-32030) and Saga compensator (-32034)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct HttpDownstreamClient {
    pool: Arc<Mutex<HashMap<String, Vec<TcpStream>>>>,
    max_payload_bytes: usize,
    connect_timeout: Duration,
    read_timeout: Duration,
    max_retries: usize,
}

impl Default for HttpDownstreamClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpDownstreamClient {
    pub fn new() -> Self {
        Self {
            pool: Arc::new(Mutex::new(HashMap::new())),
            max_payload_bytes: 10 * 1024 * 1024, // 10 MB
            connect_timeout: Duration::from_secs(3),
            read_timeout: Duration::from_secs(5),
            max_retries: 2,
        }
    }

    /// Parses URL into (host, port, path)
    pub fn parse_url(url: &str) -> Result<(String, u16, String), String> {
        let trimmed = url.trim();
        let rest = if let Some(stripped) = trimmed.strip_prefix("http://") {
            stripped
        } else {
            return Err(format!(
                "Unsupported protocol or invalid URL in downstream endpoint: '{}'. Only http:// is supported.",
                url
            ));
        };

        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };

        let (host, port) = match host_port.find(':') {
            Some(idx) => {
                let h = &host_port[..idx];
                let p = host_port[idx + 1..]
                    .parse::<u16>()
                    .map_err(|e| format!("Invalid port in '{}': {}", host_port, e))?;
                (h.to_string(), p)
            }
            None => (host_port.to_string(), 80),
        };

        Ok((host, port, path.to_string()))
    }

    /// Executes an HTTP POST request to downstream service
    pub fn post_json(
        &self,
        endpoint_url: &str,
        payload: &[u8],
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Vec<u8>, String> {
        if payload.len() > self.max_payload_bytes {
            return Err(format!(
                "Payload size {} bytes exceeds max cap {}",
                payload.len(),
                self.max_payload_bytes
            ));
        }

        let (host, port, path) = Self::parse_url(endpoint_url)?;
        let addr_key = format!("{}:{}", host, port);

        let mut last_err = String::new();
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(10 * (1 << attempt)));
            }

            match self.execute_roundtrip(&addr_key, &host, &path, payload, extra_headers) {
                Ok(resp_bytes) => return Ok(resp_bytes),
                Err(e) => {
                    last_err = e;
                }
            }
        }

        Err(format!(
            "Downstream HTTP call to '{}' failed after {} retries: {}",
            endpoint_url, self.max_retries, last_err
        ))
    }

    fn get_connection(&self, addr_key: &str) -> Result<TcpStream, String> {
        {
            let mut guard = self.pool.lock().unwrap();
            if let Some(conns) = guard.get_mut(addr_key) {
                while let Some(stream) = conns.pop() {
                    if stream
                        .set_read_timeout(Some(Duration::from_millis(1)))
                        .is_ok()
                    {
                        let _ = stream.set_read_timeout(Some(self.read_timeout));
                        return Ok(stream);
                    }
                }
            }
        }

        let socket_addrs: Vec<_> = addr_key
            .to_socket_addrs()
            .map_err(|e| format!("DNS resolution failed for '{}': {}", addr_key, e))?
            .collect();

        if socket_addrs.is_empty() {
            return Err(format!("No socket addresses resolved for '{}'", addr_key));
        }

        let stream = TcpStream::connect_timeout(&socket_addrs[0], self.connect_timeout)
            .map_err(|e| format!("Connection to '{}' failed: {}", addr_key, e))?;

        let _ = stream.set_read_timeout(Some(self.read_timeout));
        let _ = stream.set_write_timeout(Some(self.connect_timeout));
        let _ = stream.set_nodelay(true);
        Ok(stream)
    }

    fn release_connection(&self, addr_key: &str, stream: TcpStream) {
        let mut guard = self.pool.lock().unwrap();
        let conns = guard.entry(addr_key.to_string()).or_default();
        if conns.len() < 8 {
            conns.push(stream);
        }
    }

    fn execute_roundtrip(
        &self,
        addr_key: &str,
        host: &str,
        path: &str,
        payload: &[u8],
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Vec<u8>, String> {
        let mut stream = self.get_connection(addr_key)?;

        let mut req_header = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: keep-alive\r\n",
            path,
            host,
            payload.len()
        );

        if let Some(hdrs) = extra_headers {
            for (k, v) in hdrs {
                if !k.eq_ignore_ascii_case("host")
                    && !k.eq_ignore_ascii_case("content-length")
                    && !k.eq_ignore_ascii_case("connection")
                {
                    req_header.push_str(&format!("{}: {}\r\n", k, v));
                }
            }
        }
        req_header.push_str("\r\n");

        stream
            .write_all(req_header.as_bytes())
            .map_err(|e| format!("Failed to write HTTP request headers: {}", e))?;
        if !payload.is_empty() {
            stream
                .write_all(payload)
                .map_err(|e| format!("Failed to write HTTP request payload: {}", e))?;
        }
        stream
            .flush()
            .map_err(|e| format!("Failed to flush stream: {}", e))?;

        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .map_err(|e| format!("Failed to read status line: {}", e))?;

        let status_parts: Vec<&str> = status_line.split_whitespace().collect();
        if status_parts.len() < 2 {
            return Err(format!("Malformed status line: '{}'", status_line));
        }
        let status_code: u16 = status_parts[1].parse().unwrap_or(500);

        let mut content_length: Option<usize> = None;
        let mut is_chunked = false;
        let mut keep_alive = true;

        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|e| format!("Failed to read header line: {}", e))?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let k_lc = k.trim().to_lowercase();
                let v_trimmed = v.trim();
                if k_lc == "content-length" {
                    content_length = v_trimmed.parse().ok();
                } else if k_lc == "transfer-encoding" && v_trimmed.eq_ignore_ascii_case("chunked") {
                    is_chunked = true;
                } else if k_lc == "connection" && v_trimmed.eq_ignore_ascii_case("close") {
                    keep_alive = false;
                }
            }
        }

        let body = if let Some(len) = content_length {
            if len > self.max_payload_bytes {
                return Err(format!(
                    "Response Content-Length {} exceeds max cap {}",
                    len, self.max_payload_bytes
                ));
            }
            let mut buf = vec![0u8; len];
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("Failed to read response body: {}", e))?;
            buf
        } else if is_chunked {
            let mut buf = Vec::new();
            loop {
                let mut size_line = String::new();
                reader
                    .read_line(&mut size_line)
                    .map_err(|e| format!("Chunk error: {}", e))?;
                let size_str = size_line.trim().split(';').next().unwrap_or("0");
                let chunk_size = usize::from_str_radix(size_str, 16).unwrap_or(0);
                if chunk_size == 0 {
                    let mut trailer = String::new();
                    let _ = reader.read_line(&mut trailer);
                    break;
                }
                if buf.len() + chunk_size > self.max_payload_bytes {
                    return Err("Chunked body exceeded max payload cap".to_string());
                }
                let mut chunk_buf = vec![0u8; chunk_size];
                reader
                    .read_exact(&mut chunk_buf)
                    .map_err(|e| format!("Chunk data error: {}", e))?;
                buf.extend_from_slice(&chunk_buf);
                let mut crlf = [0u8; 2];
                let _ = reader.read_exact(&mut crlf);
            }
            buf
        } else {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        };

        if keep_alive {
            self.release_connection(addr_key, stream);
        }

        if status_code >= 400 {
            return Err(format!(
                "Downstream HTTP {} error: {}",
                status_code,
                String::from_utf8_lossy(&body)
            ));
        }

        Ok(body)
    }
}
