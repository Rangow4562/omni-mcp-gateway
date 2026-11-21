//! Gateway Network Listeners: Non-Blocking `tokio` + `rustls` Async TLS Stream (`AsyncTlsStream`)
//! and `--no-default-features` Bounded Thread Pool Listener (v6.5.0).
//!
//! When `async-runtime` is enabled, `run_server()` defaults to the non-blocking `tokio` listener
//! and serves both `http://` and `https://` (including mTLS and long-lived `/api/v1/events` SSE)
//! 100% asynchronously without `stream.into_std()` or `spawn_blocking` thread-pinning downgrades.

use super::auth::is_origin_allowed;
#[cfg(feature = "tls-rustls")]
use super::http_codec::build_inbound_tls_config;
use super::http_codec::{get_header, read_line_capped, send_raw_error_response};
#[cfg(feature = "async-runtime")]
use super::sse::stream_sse_async;
use super::sse::stream_sse_sync;
use super::{get_gateway_context, AutoSyncScheduler, ProductionGatewayContext};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
#[cfg(not(feature = "async-runtime"))]
use std::net::TcpListener;
use std::net::TcpStream;
#[cfg(not(feature = "async-runtime"))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(not(feature = "async-runtime"))]
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::Duration;

/// 100% Non-blocking, `#![forbid(unsafe_code)]` async TLS server stream bridging
/// `tokio::net::TcpStream` and `rustls::ServerConnection` sans-I/O state machine.
#[cfg(all(feature = "async-runtime", feature = "tls-rustls"))]
pub struct AsyncTlsStream {
    tcp: tokio::net::TcpStream,
    conn: rustls::ServerConnection,
    write_buf: Vec<u8>,
    write_pos: usize,
    peer_eof: bool,
    close_notify_sent: bool,
}

#[cfg(all(feature = "async-runtime", feature = "tls-rustls"))]
impl AsyncTlsStream {
    /// Performs the TLS 1.2/1.3 (and optional mTLS) handshake completely asynchronously.
    pub async fn accept(
        mut tcp: tokio::net::TcpStream,
        config: Arc<rustls::ServerConfig>,
    ) -> std::io::Result<Self> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut conn = rustls::ServerConnection::new(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let mut net_buf = [0u8; 8192];
        let mut out_buf = Vec::with_capacity(4096);

        while conn.is_handshaking() {
            while conn.wants_write() {
                out_buf.clear();
                conn.write_tls(&mut out_buf)?;
                if !out_buf.is_empty() {
                    tcp.write_all(&out_buf).await?;
                }
            }
            tcp.flush().await?;

            if !conn.is_handshaking() {
                break;
            }

            if conn.wants_read() {
                let n = tcp.read(&mut net_buf).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "TLS peer closed connection during handshake",
                    ));
                }
                let mut cursor = std::io::Cursor::new(&net_buf[..n]);
                conn.read_tls(&mut cursor)?;
                conn.process_new_packets()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            }
        }

        // Flush any final handshake messages (e.g., Finished / NewSessionTicket)
        while conn.wants_write() {
            out_buf.clear();
            conn.write_tls(&mut out_buf)?;
            if !out_buf.is_empty() {
                tcp.write_all(&out_buf).await?;
            }
        }
        tcp.flush().await?;

        Ok(Self {
            tcp,
            conn,
            write_buf: Vec::new(),
            write_pos: 0,
            peer_eof: false,
            close_notify_sent: false,
        })
    }

    fn drain_tls_writes(&mut self) -> std::io::Result<()> {
        if self.write_pos >= self.write_buf.len() {
            self.write_buf.clear();
            self.write_pos = 0;
        }
        while self.conn.wants_write() {
            let n = self.conn.write_tls(&mut self.write_buf)?;
            if n == 0 {
                break;
            }
        }
        Ok(())
    }

    fn poll_flush_write_buf(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::pin::Pin;
        use std::task::Poll;
        use tokio::io::AsyncWrite;

        self.drain_tls_writes()?;
        while self.write_pos < self.write_buf.len() {
            let slice = &self.write_buf[self.write_pos..];
            match Pin::new(&mut self.tcp).poll_write(cx, slice) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "TLS socket closed while writing",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    self.write_pos += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.write_buf.clear();
        self.write_pos = 0;
        Poll::Ready(Ok(()))
    }
}

#[cfg(all(feature = "async-runtime", feature = "tls-rustls"))]
impl tokio::io::AsyncRead for AsyncTlsStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::pin::Pin;
        use std::task::Poll;

        let this = &mut *self;
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            // 1. Try reading already-decrypted plaintext first
            let mut tmp = vec![0u8; buf.remaining().min(16384)];
            match this.conn.reader().read(&mut tmp) {
                Ok(0) => {
                    // rustls returns Ok(0) on a non-empty buffer only after clean TLS close_notify
                    this.peer_eof = true;
                    return Poll::Ready(Ok(()));
                }
                Ok(n) => {
                    buf.put_slice(&tmp[..n]);
                    return Poll::Ready(Ok(()));
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Poll::Ready(Err(e)),
            }

            // Flush any TLS protocol control records (e.g. key update / alerts) if pending
            if this.conn.wants_write() || this.write_pos < this.write_buf.len() {
                match this.poll_flush_write_buf(cx) {
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending if !this.conn.wants_read() => return Poll::Pending,
                    _ => {}
                }
            }

            if this.peer_eof {
                return Poll::Ready(Ok(()));
            }

            // 2. Poll underlying TcpStream for more encrypted TLS records
            let mut raw = [0u8; 8192];
            let mut rbuf = tokio::io::ReadBuf::new(&mut raw);
            match Pin::new(&mut this.tcp).poll_read(cx, &mut rbuf) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {
                    let filled = rbuf.filled();
                    if filled.is_empty() {
                        this.peer_eof = true;
                        // Drain any final decrypted bytes on EOF
                        let mut final_tmp = vec![0u8; buf.remaining().min(16384)];
                        match this.conn.reader().read(&mut final_tmp) {
                            Ok(n) if n > 0 => buf.put_slice(&final_tmp[..n]),
                            _ => {}
                        }
                        return Poll::Ready(Ok(()));
                    }
                    let mut cursor = std::io::Cursor::new(filled);
                    while (cursor.position() as usize) < filled.len() {
                        let read_n = this.conn.read_tls(&mut cursor)?;
                        if read_n == 0 {
                            break;
                        }
                        this.conn
                            .process_new_packets()
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    }
                }
            }
        }
    }
}

#[cfg(all(feature = "async-runtime", feature = "tls-rustls"))]
impl tokio::io::AsyncWrite for AsyncTlsStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;

        let this = &mut *self;
        if this.write_pos < this.write_buf.len() {
            match this.poll_flush_write_buf(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
        }
        let written = this.conn.writer().write(buf)?;
        this.drain_tls_writes()?;
        let _ = this.poll_flush_write_buf(cx)?;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::pin::Pin;
        use std::task::Poll;

        let this = &mut *self;
        match this.poll_flush_write_buf(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        Pin::new(&mut this.tcp).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::pin::Pin;
        use std::task::Poll;

        let this = &mut *self;
        if !this.close_notify_sent {
            this.conn.send_close_notify();
            this.close_notify_sent = true;
        }
        match this.poll_flush_write_buf(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        Pin::new(&mut this.tcp).poll_shutdown(cx)
    }
}

pub fn run_server(host: &str, port: u16) -> Result<(), String> {
    let ctx = get_gateway_context().clone();
    ctx.config.validate_bind(host)?;
    ctx.cache.start_background_refresh();

    // Opt-in background AutoSyncScheduler for external MCP servers with sync_interval_sec
    {
        let sync_ctx = ctx.clone();
        thread::spawn(move || {
            let mut scheduler = AutoSyncScheduler::new();
            loop {
                thread::sleep(Duration::from_secs(10));
                let now = crate::logging::now_epoch_secs();
                scheduler.tick(&sync_ctx, now);
            }
        });
    }

    let bind_addr = format!("{}:{}", host, port);

    #[cfg(feature = "tls-rustls")]
    let tls_config = build_inbound_tls_config()?;

    let scheme = {
        #[cfg(feature = "tls-rustls")]
        {
            if tls_config.is_some() {
                "https"
            } else {
                "http"
            }
        }
        #[cfg(not(feature = "tls-rustls"))]
        {
            "http"
        }
    };

    println!(
        "[*] Omni MCP Gateway running at {}://{}:{}",
        scheme, host, port
    );
    println!("[*] Liveness probe: {}://{}:{}/healthz", scheme, host, port);
    println!(
        "[*] Prometheus metrics: {}://{}:{}/metrics",
        scheme, host, port
    );
    println!(
        "[*] Omni MCP proxy available at {}://{}:{}/omni/mcp",
        scheme, host, port
    );
    println!(
        "[*] Dynamic virtual servers available at {}://{}:{}/<service>/mcp",
        scheme, host, port
    );

    #[cfg(feature = "async-runtime")]
    {
        run_async_server(
            &bind_addr,
            Arc::new(ctx),
            #[cfg(feature = "tls-rustls")]
            tls_config,
        )
    }

    #[cfg(not(feature = "async-runtime"))]
    {
        let listener = TcpListener::bind(&bind_addr)
            .map_err(|e| format!("Failed to bind HTTP server on {}: {}", bind_addr, e))?;
        let _ = listener.set_nonblocking(true);
        let shutdown = Arc::new(AtomicBool::new(false));

        let num_workers = ctx.config.worker_threads.max(4);
        let queue_bound = (num_workers * 16).max(64);
        let (tx, rx) = mpsc::sync_channel::<TcpStream>(queue_bound);
        let shared_rx = Arc::new(Mutex::new(rx));
        let mut worker_handles = Vec::with_capacity(num_workers);

        for _ in 0..num_workers {
            let worker_rx = shared_rx.clone();
            let worker_ctx = ctx.clone();
            #[cfg(feature = "tls-rustls")]
            let worker_tls = tls_config.clone();
            worker_handles.push(thread::spawn(move || loop {
                let stream = {
                    let lock = worker_rx.lock().unwrap();
                    match lock.recv() {
                        Ok(s) => s,
                        Err(_) => break,
                    }
                };
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(feature = "tls-rustls")]
                    if let Some(ref cfg) = worker_tls {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
                        if let Ok(conn) = rustls::ServerConnection::new(cfg.clone()) {
                            let tls_stream = rustls::StreamOwned::new(conn, stream);
                            handle_stream(tls_stream, &worker_ctx);
                        }
                        return;
                    }
                    let _ = stream.set_nonblocking(false);
                    handle_connection(stream, &worker_ctx);
                }));
            }));
        }

        while !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    match tx.try_send(stream) {
                        Ok(_) => {}
                        Err(mpsc::TrySendError::Full(mut saturated_stream)) => {
                            let _ = saturated_stream.set_nonblocking(false);
                            send_raw_error_response(
                                &mut saturated_stream,
                                503,
                                "Service Unavailable",
                                "Service Unavailable: Worker accept queue saturated",
                            );
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }

        drop(tx);
        for h in worker_handles {
            let _ = h.join();
        }

        Ok(())
    }
}

#[cfg(feature = "async-runtime")]
fn run_async_server(
    bind_addr: &str,
    ctx: Arc<ProductionGatewayContext>,
    #[cfg(feature = "tls-rustls")] tls_config: Option<Arc<rustls::ServerConfig>>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(ctx.config.worker_threads.max(4))
        .build()
        .map_err(|e| format!("Failed to build tokio runtime: {}", e))?;

    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| format!("Failed to bind async HTTP server on {}: {}", bind_addr, e))?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let ctrl_c_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            let _ = ctrl_c_tx.send(true);
        });
        serve_async_listener(
            listener,
            ctx,
            #[cfg(feature = "tls-rustls")]
            tls_config,
            shutdown_rx,
        )
        .await;
        Ok(())
    })
}

#[cfg(feature = "async-runtime")]
pub async fn serve_async_listener(
    listener: tokio::net::TcpListener,
    ctx: Arc<ProductionGatewayContext>,
    #[cfg(feature = "tls-rustls")] tls_config: Option<Arc<rustls::ServerConfig>>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let max_conns = (ctx.config.worker_threads.max(4) * 64).max(256);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_conns));
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _)) => {
                        let _ = stream.set_nodelay(true);
                        let permit = match semaphore.clone().try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                let mut s = stream;
                                use tokio::io::AsyncWriteExt;
                                let body = b"Service Unavailable: Worker accept queue saturated";
                                let resp = format!(
                                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                );
                                let _ = s.write_all(resp.as_bytes()).await;
                                let _ = s.write_all(body).await;
                                continue;
                            }
                        };
                        let conn_ctx = ctx.clone();
                        let conn_shutdown = shutdown_rx.clone();
                        #[cfg(feature = "tls-rustls")]
                        let conn_tls = tls_config.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            #[cfg(feature = "tls-rustls")]
                            if let Some(cfg) = conn_tls {
                                // Native non-blocking async TLS handshake & streaming (v6.5.0)
                                if let Ok(Ok(tls_stream)) = tokio::time::timeout(
                                    Duration::from_secs(10),
                                    AsyncTlsStream::accept(stream, cfg),
                                )
                                .await
                                {
                                    handle_async_stream(tls_stream, conn_ctx, conn_shutdown).await;
                                }
                                return;
                            }
                            handle_async_stream(stream, conn_ctx, conn_shutdown).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        semaphore.acquire_many(max_conns as u32),
    )
    .await;
}

#[cfg(feature = "async-runtime")]
async fn read_line_capped_async<R>(
    reader: &mut tokio::io::BufReader<R>,
    buf: &mut String,
    max_len: usize,
) -> std::io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let mut total = 0usize;
    let mut raw = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if total + take > max_len {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Line exceeds maximum allowed length",
                ));
            }
            raw.extend_from_slice(&available[..take]);
            reader.consume(take);
            total += take;
            break;
        }
        let avail_len = available.len();
        if total + avail_len > max_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Line exceeds maximum allowed length",
            ));
        }
        raw.extend_from_slice(available);
        reader.consume(avail_len);
        total += avail_len;
    }
    buf.push_str(&String::from_utf8_lossy(&raw));
    Ok(total)
}

#[cfg(feature = "async-runtime")]
async fn send_raw_error_response_async<W>(writer: &mut W, code: u16, reason: &str, msg: &str)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        code,
        reason,
        msg.len(),
        msg
    );
    let _ = writer.write_all(resp.as_bytes()).await;
    let _ = writer.flush().await;
}

#[cfg(feature = "async-runtime")]
pub async fn handle_async_stream<S>(
    stream: S,
    ctx: Arc<ProductionGatewayContext>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    let mut reader = BufReader::new(stream);
    let mut request_count = 0usize;
    let max_requests_per_conn = 100usize;

    loop {
        if *shutdown_rx.borrow() || request_count >= max_requests_per_conn {
            break;
        }
        let mut request_line = String::new();
        let read_res = tokio::time::timeout(
            Duration::from_secs(15),
            read_line_capped_async(&mut reader, &mut request_line, 8192),
        )
        .await;
        match read_res {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                send_raw_error_response_async(
                    reader.get_mut(),
                    431,
                    "Request Header Fields Too Large",
                    "Request line exceeds 8KB cap",
                )
                .await;
                return;
            }
            Ok(Err(_)) => break,
            Ok(Ok(_)) => {}
        }
        request_count += 1;
        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            break;
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = HashMap::new();
        let mut content_length: usize = 0;
        let mut client_wants_close = false;
        let mut header_count = 0usize;
        let mut total_header_bytes = 0usize;
        let mut seen_content_length = false;
        let mut seen_transfer_encoding = false;
        let mut smuggling_detected = false;
        let mut malformed_content_length = false;

        loop {
            let mut h_line = String::new();
            let h_res = tokio::time::timeout(
                Duration::from_secs(15),
                read_line_capped_async(&mut reader, &mut h_line, 8192),
            )
            .await;
            match h_res {
                Ok(Ok(0)) | Err(_) => return,
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidData => {
                    send_raw_error_response_async(
                        reader.get_mut(),
                        431,
                        "Request Header Fields Too Large",
                        "Request Header Fields Too Large (Line Cap: 8KB)",
                    )
                    .await;
                    return;
                }
                Ok(Err(_)) => return,
                Ok(Ok(_)) => {}
            }
            header_count += 1;
            total_header_bytes += h_line.len();
            if header_count > 100 || total_header_bytes > 64 * 1024 {
                send_raw_error_response_async(
                    reader.get_mut(),
                    431,
                    "Request Header Fields Too Large",
                    "Request Header Fields Too Large (Cap: 64KB)",
                )
                .await;
                return;
            }
            let trimmed = h_line.trim_end_matches(&['\r', '\n'][..]);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let kl = k.trim().to_ascii_lowercase();
                let val = v.trim().to_string();
                if kl == "content-length" {
                    if seen_content_length && headers.get("content-length") != Some(&val) {
                        smuggling_detected = true;
                    }
                    seen_content_length = true;
                    let val_trimmed = val.trim();
                    if val_trimmed.is_empty() || !val_trimmed.chars().all(|c| c.is_ascii_digit()) {
                        malformed_content_length = true;
                    } else {
                        match val_trimmed.parse::<usize>() {
                            Ok(cl) => content_length = cl,
                            Err(_) => malformed_content_length = true,
                        }
                    }
                } else if kl == "transfer-encoding" {
                    seen_transfer_encoding = true;
                } else if kl == "connection" && val.eq_ignore_ascii_case("close") {
                    client_wants_close = true;
                }
                headers.insert(kl, val);
            }
        }

        if malformed_content_length {
            send_raw_error_response_async(
                reader.get_mut(),
                400,
                "Bad Request",
                "Bad Request: Invalid Content-Length",
            )
            .await;
            return;
        }

        if smuggling_detected || (seen_transfer_encoding && seen_content_length) {
            send_raw_error_response_async(
                reader.get_mut(),
                400,
                "Bad Request",
                "Bad Request: Conflicting Content-Length / Transfer-Encoding",
            )
            .await;
            return;
        }

        if seen_transfer_encoding && !seen_content_length {
            send_raw_error_response_async(
                reader.get_mut(),
                501,
                "Not Implemented",
                "Not Implemented: Transfer-Encoding not supported",
            )
            .await;
            return;
        }

        if content_length > ctx.config.max_body_size_bytes {
            send_raw_error_response_async(
                reader.get_mut(),
                413,
                "Payload Too Large",
                "Payload Too Large (Exceeds Limit)",
            )
            .await;
            return;
        }

        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 {
            let body_res =
                tokio::time::timeout(Duration::from_secs(30), reader.read_exact(&mut body_bytes))
                    .await;
            if !matches!(body_res, Ok(Ok(_))) {
                return;
            }
        }

        let path_base = path.split('?').next().unwrap_or(&path).to_string();
        let is_sse_route =
            method == "GET" && (path_base == "/api/events" || path_base == "/api/v1/events");

        let ctx_c = ctx.clone();
        let m_c = method.clone();
        let p_c = path.clone();
        let h_c = headers.clone();
        let (status, content_type, extra_headers, resp_body) =
            match tokio::task::spawn_blocking(move || {
                ctx_c.handle_http(&m_c, &p_c, &h_c, &body_bytes)
            })
            .await
            {
                Ok(res) => res,
                Err(_) => return,
            };

        if is_sse_route && status == 200 {
            let w = reader.get_mut();
            stream_sse_async(w, &ctx, &headers, &resp_body, &mut shutdown_rx).await;
            return;
        }

        let status_text = match status {
            200 => "OK",
            202 => "Accepted",
            204 => "No Content",
            302 => "Found",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            410 => "Gone",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "OK",
        };
        let conn_header = if client_wants_close || request_count >= max_requests_per_conn {
            "close"
        } else {
            "keep-alive"
        };
        let mut response_header = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
            status,
            status_text,
            content_type,
            resp_body.len(),
            conn_header
        );
        for (ek, ev) in extra_headers {
            response_header.push_str(&format!("{}: {}\r\n", ek, ev));
        }
        if let Some(origin) = get_header(&headers, "origin") {
            if is_origin_allowed(
                origin,
                &ctx.config.cors_allowed_origins,
                get_header(&headers, "host"),
            ) {
                if ctx.config.cors_allowed_origins == "*" {
                    response_header.push_str("Access-Control-Allow-Origin: *\r\n");
                } else {
                    response_header.push_str(&format!(
                        "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                        origin
                    ));
                }
                response_header.push_str(
                    "Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS\r\n",
                );
                response_header.push_str(
                    "Access-Control-Allow-Headers: Content-Type, Authorization, Mcp-Session-Id, Mcp-Protocol-Version, Last-Event-ID, traceparent, tracestate\r\n",
                );
            }
        }
        response_header.push_str("\r\n");

        let w = reader.get_mut();
        if w.write_all(response_header.as_bytes()).await.is_err()
            || (!resp_body.is_empty() && w.write_all(&resp_body).await.is_err())
            || w.flush().await.is_err()
        {
            break;
        }
        if conn_header == "close" {
            break;
        }
    }
}

pub fn handle_connection(stream: TcpStream, ctx: &ProductionGatewayContext) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    handle_stream(stream, ctx);
}

pub fn handle_stream<S: Read + Write>(stream: S, ctx: &ProductionGatewayContext) {
    let mut reader = BufReader::new(stream);

    loop {
        let mut request_line = String::new();
        match read_line_capped(&mut reader, &mut request_line, 8192) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return;
        }
        let method = parts[0].to_string();
        let path = parts[1].to_string();

        let mut headers = HashMap::new();
        let mut content_length = 0usize;
        let mut keep_alive = true;
        let mut header_lines = 0usize;
        let mut total_header_bytes = 0usize;
        let mut seen_content_length = false;
        let mut seen_transfer_encoding = false;
        let mut smuggling_detected = false;
        let mut malformed_content_length = false;

        loop {
            let mut line = String::new();
            match read_line_capped(&mut reader, &mut line, 8192) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    header_lines += 1;
                    total_header_bytes += line.len();
                    if header_lines > 100 || total_header_bytes > 64 * 1024 {
                        send_raw_error_response(
                            reader.get_mut(),
                            431,
                            "Request Header Fields Too Large",
                            "Request Header Fields Too Large (Cap: 64KB)",
                        );
                        return;
                    }
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = trimmed.split_once(':') {
                        let key_lc = k.trim().to_lowercase();
                        let val_str = v.trim().to_string();
                        if key_lc == "content-length" {
                            if seen_content_length
                                && headers.get("content-length") != Some(&val_str)
                            {
                                smuggling_detected = true;
                            }
                            seen_content_length = true;
                            let val_trimmed = val_str.trim();
                            if val_trimmed.is_empty()
                                || !val_trimmed.chars().all(|c| c.is_ascii_digit())
                            {
                                malformed_content_length = true;
                            } else {
                                match val_trimmed.parse::<usize>() {
                                    Ok(cl) => content_length = cl,
                                    Err(_) => malformed_content_length = true,
                                }
                            }
                        } else if key_lc == "transfer-encoding" {
                            seen_transfer_encoding = true;
                        } else if key_lc == "connection" && val_str.eq_ignore_ascii_case("close") {
                            keep_alive = false;
                        }
                        headers.insert(key_lc, val_str);
                    }
                }
            }
        }

        if malformed_content_length {
            send_raw_error_response(
                reader.get_mut(),
                400,
                "Bad Request",
                "Bad Request: Invalid Content-Length",
            );
            return;
        }

        if smuggling_detected || (seen_transfer_encoding && seen_content_length) {
            send_raw_error_response(
                reader.get_mut(),
                400,
                "Bad Request",
                "Bad Request: Conflicting Content-Length / Transfer-Encoding",
            );
            return;
        }

        if seen_transfer_encoding && !seen_content_length {
            send_raw_error_response(
                reader.get_mut(),
                501,
                "Not Implemented",
                "Not Implemented: Transfer-Encoding not supported",
            );
            return;
        }

        if content_length > ctx.config.max_body_size_bytes {
            send_raw_error_response(
                reader.get_mut(),
                413,
                "Payload Too Large",
                "Payload Too Large (Exceeds Limit)",
            );
            return;
        }

        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
            return;
        }

        let path_base = path.split('?').next().unwrap_or(&path);
        let is_sse_route =
            method == "GET" && (path_base == "/api/events" || path_base == "/api/v1/events");

        let (status, content_type, extra_headers, resp_body) =
            ctx.handle_http(&method, &path, &headers, &body_bytes);

        if is_sse_route && status == 200 {
            stream_sse_sync(reader.get_mut(), ctx, &headers, &resp_body);
            return;
        }

        let status_text = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        };

        let conn_hdr = if keep_alive && status < 400 {
            "keep-alive"
        } else {
            "close"
        };
        let mut header_str = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            status,
            status_text,
            content_type,
            resp_body.len(),
        );

        if let Some(origin) = get_header(&headers, "origin") {
            if status != 403
                && is_origin_allowed(
                    origin,
                    &ctx.config.cors_allowed_origins,
                    get_header(&headers, "host"),
                )
            {
                if ctx.config.cors_allowed_origins == "*" {
                    header_str.push_str("Access-Control-Allow-Origin: *\r\n");
                } else {
                    header_str.push_str(&format!(
                        "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                        origin
                    ));
                }
                header_str.push_str("Access-Control-Allow-Methods: GET, POST, DELETE, OPTIONS\r\n");
                header_str
                    .push_str("Access-Control-Allow-Headers: Content-Type, Authorization\r\n");
            }
        }

        header_str.push_str(&format!("Connection: {}\r\n", conn_hdr));
        for (hk, hv) in extra_headers {
            header_str.push_str(&format!("{}: {}\r\n", hk, hv));
        }
        header_str.push_str("\r\n");

        if reader.get_mut().write_all(header_str.as_bytes()).is_err() {
            return;
        }
        if reader.get_mut().write_all(&resp_body).is_err() {
            return;
        }
        let _ = reader.get_mut().flush();

        if !keep_alive || status >= 400 {
            return;
        }
    }
}
