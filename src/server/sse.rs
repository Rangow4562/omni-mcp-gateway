//! Sync & Non-Blocking Async Server-Sent Events (SSE) Streaming for `/api/v1/events` (v6.5.0).

use super::auth::is_origin_allowed;
use super::http_codec::get_header;
use super::ProductionGatewayContext;
use std::collections::HashMap;
use std::io::Write;
use std::thread;
use std::time::Duration;

fn build_sse_response_headers(
    ctx: &ProductionGatewayContext,
    headers: &HashMap<String, String>,
) -> String {
    let mut hdr = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: close\r\nX-Accel-Buffering: no\r\n",
    );
    if let Some(origin) = get_header(headers, "origin") {
        if is_origin_allowed(
            origin,
            &ctx.config.cors_allowed_origins,
            get_header(headers, "host"),
        ) {
            if ctx.config.cors_allowed_origins == "*" {
                hdr.push_str("Access-Control-Allow-Origin: *\r\n");
            } else {
                hdr.push_str(&format!(
                    "Access-Control-Allow-Origin: {}\r\nVary: Origin\r\n",
                    origin
                ));
            }
        }
    }
    hdr.push_str("\r\n");
    hdr
}

/// Synchronous SSE event stream writer for `--no-default-features` / blocking stream handlers.
pub(crate) fn stream_sse_sync<W: Write>(
    writer: &mut W,
    ctx: &ProductionGatewayContext,
    headers: &HashMap<String, String>,
    initial_body: &[u8],
) {
    let hdr = build_sse_response_headers(ctx, headers);
    if writer.write_all(hdr.as_bytes()).is_err()
        || writer.write_all(initial_body).is_err()
        || writer.flush().is_err()
    {
        return;
    }

    let mut last_seq = ctx.event_bus.latest_seq();
    let mut idle_ticks = 0u32;
    loop {
        thread::sleep(Duration::from_millis(500));
        let new_events = ctx.event_bus.since(last_seq);
        if !new_events.is_empty() {
            idle_ticks = 0;
            for ev in new_events {
                last_seq = ev.seq;
                let frame = format!(
                    "id: {}\nevent: {}\ndata: {}\n\n",
                    ev.seq,
                    ev.event_type,
                    ev.payload.to_string_compact()
                );
                if writer.write_all(frame.as_bytes()).is_err() || writer.flush().is_err() {
                    return;
                }
            }
        } else {
            idle_ticks += 1;
            if idle_ticks >= 30 {
                idle_ticks = 0;
                if writer.write_all(b": keepalive\n\n").is_err() || writer.flush().is_err() {
                    return;
                }
            }
        }
    }
}

/// Non-blocking `tokio` async SSE event stream writer supporting both HTTP and native async HTTPS/mTLS.
#[cfg(feature = "async-runtime")]
pub(crate) async fn stream_sse_async<W>(
    writer: &mut W,
    ctx: &ProductionGatewayContext,
    headers: &HashMap<String, String>,
    initial_body: &[u8],
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let hdr = build_sse_response_headers(ctx, headers);
    if writer.write_all(hdr.as_bytes()).await.is_err()
        || writer.write_all(initial_body).await.is_err()
        || writer.flush().await.is_err()
    {
        return;
    }

    let mut last_seq = ctx.event_bus.latest_seq();
    let mut idle_ticks = 0u32;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                let new_events = ctx.event_bus.since(last_seq);
                if !new_events.is_empty() {
                    idle_ticks = 0;
                    for ev in new_events {
                        last_seq = ev.seq;
                        let frame = format!(
                            "id: {}\nevent: {}\ndata: {}\n\n",
                            ev.seq,
                            ev.event_type,
                            ev.payload.to_string_compact()
                        );
                        if writer.write_all(frame.as_bytes()).await.is_err()
                            || writer.flush().await.is_err()
                        {
                            return;
                        }
                    }
                } else {
                    idle_ticks += 1;
                    if idle_ticks >= 75 {
                        idle_ticks = 0;
                        if writer.write_all(b": keepalive\n\n").await.is_err()
                            || writer.flush().await.is_err()
                        {
                            return;
                        }
                    }
                }
            }
        }
    }
}
