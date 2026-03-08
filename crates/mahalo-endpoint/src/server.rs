use std::cell::UnsafeCell;
use std::net::SocketAddr;
use std::rc::Rc;

use rebar_core::io::{BufResult, TcpListener, TcpStream};

use mahalo_core::conn::{SseKeepAliveKey, SseStreamKey};
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;

use crate::endpoint::{ErrorHandler, WsConfig};
use crate::http_parse::{self, ParseError};

/// Default buffer size for read operations (lease and Vec fallback).
const DEFAULT_BUF_SIZE: usize = 8192;

// ---------------------------------------------------------------------------
// MahaloStream — abstracts over plain TCP and TLS-encrypted connections
// ---------------------------------------------------------------------------

/// A connection stream that is either plain TCP or TLS-encrypted.
///
/// `MahaloStream::Plain` delegates directly to rebar's `TcpStream` methods
/// (zero overhead). `MahaloStream::Tls` uses compio-io `AsyncRead`/`AsyncWrite`
/// through `compio_tls::TlsStream`, wrapped in `UnsafeCell` for interior
/// mutability (safe because we run on a single-threaded executor).
pub(crate) enum MahaloStream {
    Plain(TcpStream),
    Tls(UnsafeCell<compio_tls::TlsStream<TcpStream>>),
}

impl MahaloStream {
    /// Read into a Vec buffer. For Plain, delegates to TcpStream::read.
    /// For TLS, uses compio-io AsyncRead.
    pub async fn read(&self, mut buf: Vec<u8>) -> BufResult<usize, Vec<u8>> {
        match self {
            MahaloStream::Plain(s) => s.read(buf).await,
            MahaloStream::Tls(cell) => {
                use compio_io::AsyncRead;
                // compio-io reads into available capacity (cap - len). The caller
                // may pass a Vec with len > 0 (e.g. from split_off). Truncate to
                // 0 so the full capacity is available for the read, matching
                // rebar TcpStream::read semantics which ignore len.
                buf.clear();
                // SAFETY: RebarExecutor is a single-threaded, cooperative executor.
                // Only one task runs at a time, so no concurrent access to the
                // TlsStream is possible. The UnsafeCell is never shared across threads.
                let s = unsafe { &mut *cell.get() };
                AsyncRead::read(s, buf).await
            }
        }
    }

    /// Write all bytes. For Plain, delegates to TcpStream::write_all.
    /// For TLS, uses compio-io AsyncWriteExt::write_all.
    pub async fn write_all(&self, buf: Vec<u8>) -> BufResult<(), Vec<u8>> {
        match self {
            MahaloStream::Plain(s) => s.write_all(buf).await,
            MahaloStream::Tls(cell) => {
                use compio_io::{AsyncWrite, AsyncWriteExt};
                // SAFETY: RebarExecutor is a single-threaded, cooperative executor.
                // Only one task runs at a time, so no concurrent access to the
                // TlsStream is possible. The UnsafeCell is never shared across threads.
                let s = unsafe { &mut *cell.get() };
                match AsyncWriteExt::write_all(s, buf).await {
                    BufResult(Ok(_n), b) => {
                        // Flush TLS record buffer so data reaches the peer.
                        if let Err(e) = AsyncWrite::flush(s).await {
                            return BufResult(Err(e), b);
                        }
                        BufResult(Ok(()), b)
                    }
                    BufResult(Err(e), b) => BufResult(Err(e), b),
                }
            }
        }
    }

    /// Try to lease a buffer and read (zero-copy). Returns None for TLS
    /// since TLS decryption necessarily copies data.
    pub async fn read_lease(
        &self,
        lease: turbine_core::buffer::leased::LeasedBuffer,
    ) -> Option<BufResult<usize, turbine_core::buffer::leased::LeasedBuffer>> {
        match self {
            MahaloStream::Plain(s) => Some(s.read_lease(lease).await),
            MahaloStream::Tls(_) => None, // Falls through to Vec path
        }
    }

    /// Returns true if this is a TLS connection.
    pub fn is_tls(&self) -> bool {
        matches!(self, MahaloStream::Tls(_))
    }
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

/// Run the accept loop on the current executor using an existing listener.
pub(crate) async fn run_accept_loop(
    listener: TcpListener,
    router: MahaloRouter,
    error_handler: Option<ErrorHandler>,
    after_plugs: Vec<Box<dyn Plug>>,
    runtime: Rc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
    tls_acceptor: Option<rebar_core::tls::TlsAcceptor>,
) {
    let router = Rc::new(router);
    let error_handler = Rc::new(error_handler);
    let after_plugs: Rc<Vec<Box<dyn Plug>>> = Rc::new(after_plugs);
    let ws_config = Rc::new(ws_config);
    let tls_acceptor = Rc::new(tls_acceptor);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let router = Rc::clone(&router);
                let error_handler = Rc::clone(&error_handler);
                let after_plugs = Rc::clone(&after_plugs);
                let runtime = Rc::clone(&runtime);
                let ws_config = Rc::clone(&ws_config);
                let tls_acceptor = Rc::clone(&tls_acceptor);

                rebar_core::executor::spawn(async move {
                    // TLS handshake (if configured) happens inside the spawned
                    // task, not blocking the accept loop.
                    let stream = match tls_acceptor.as_ref() {
                        Some(acc) => match acc.accept(stream).await {
                            Ok(tls) => MahaloStream::Tls(UnsafeCell::new(tls)),
                            Err(e) => {
                                tracing::warn!("TLS handshake failed: {e}");
                                return;
                            }
                        },
                        None => MahaloStream::Plain(stream),
                    };

                    handle_connection(
                        stream,
                        peer_addr,
                        &router,
                        &error_handler,
                        &after_plugs,
                        &runtime,
                        body_limit,
                        &ws_config,
                    )
                    .await;
                })
                .detach();
            }
            Err(e) => {
                tracing::warn!("accept error: {e}");
            }
        }
    }
}

/// Handle a single connection, supporting HTTP keep-alive and WebSocket upgrade.
///
/// Uses a hybrid read strategy:
/// - **Fast path**: When a turbine buffer pool is available and the connection is
///   plain TCP, lease an arena buffer for reads (zero-alloc for single-read requests).
/// - **Fallback path**: When no pool, TLS (decryption copies anyway), or partial
///   requests span multiple reads, uses a heap-allocated `Vec<u8>`.
async fn handle_connection(
    stream: MahaloStream,
    peer_addr: SocketAddr,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    runtime: &Rc<Runtime>,
    body_limit: usize,
    ws_config: &Option<WsConfig>,
) {
    let mut resp_buf = Vec::with_capacity(256);

    // Fallback accumulation buffer — allocated lazily only when needed.
    // (buf, filled) where filled is the number of valid bytes in buf.
    let mut accum: Option<(Vec<u8>, usize)> = None;

    loop {
        // If we have accumulated data from a previous partial read, use Vec path.
        if let Some((ref mut buf, ref mut filled)) = accum {
            if !vec_read_loop(
                &stream, peer_addr, router, error_handler, after_plugs,
                runtime, body_limit, ws_config, buf, filled, &mut resp_buf,
            )
            .await
            {
                return;
            }
            // If we fully drained accum, switch back to lease path.
            if *filled == 0 {
                accum = None;
            }
            continue;
        }

        // TLS connections skip the lease path (decryption copies anyway).
        if stream.is_tls() {
            accum = Some((vec![0u8; DEFAULT_BUF_SIZE], 0));
            continue;
        }

        // Fast path: lease a buffer, read, try to parse in one shot.
        let lease = rebar_core::executor::with_buffer_pool(|pool| pool.lease(DEFAULT_BUF_SIZE)).flatten();

        match lease {
            Some(lease) => {
                let BufResult(result, lease) = match stream.read_lease(lease).await {
                    Some(r) => r,
                    None => {
                        // Shouldn't happen for Plain, but fall back gracefully.
                        accum = Some((vec![0u8; DEFAULT_BUF_SIZE], 0));
                        continue;
                    }
                };
                let n = match result {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => return,
                };

                // Try to parse directly from leased memory.
                match http_parse::try_parse_request(
                    &lease.as_slice()[..n], body_limit, peer_addr,
                ) {
                    Ok(Some(parsed)) => {
                        let bytes_consumed = parsed.bytes_consumed;
                        let leftover = n - bytes_consumed;

                        // Copy leftover bytes before passing parsed (which moves).
                        let leftover_data = if leftover > 0 {
                            Some(lease.as_slice()[bytes_consumed..n].to_vec())
                        } else {
                            None
                        };

                        // Execute the request — lease can be dropped after this.
                        if !execute_and_respond(
                            &stream, router, error_handler, after_plugs,
                            runtime, ws_config, parsed, &mut resp_buf,
                        )
                        .await
                        {
                            return;
                        }

                        if let Some(mut leftover_bytes) = leftover_data {
                            // Pipelining: reuse the to_vec() allocation directly.
                            let filled = leftover_bytes.len();
                            if leftover_bytes.capacity() < DEFAULT_BUF_SIZE {
                                leftover_bytes.reserve(DEFAULT_BUF_SIZE - leftover_bytes.len());
                            }
                            leftover_bytes.resize(leftover_bytes.capacity(), 0);
                            accum = Some((leftover_bytes, filled));
                        }
                        // lease drops here — zero-alloc for the common case.
                    }
                    Ok(None) => {
                        // Incomplete — copy to accum, fall back to Vec path.
                        let cap = DEFAULT_BUF_SIZE.max(n * 2);
                        let mut buf = vec![0u8; cap];
                        buf[..n].copy_from_slice(&lease.as_slice()[..n]);
                        accum = Some((buf, n));
                    }
                    Err(ParseError::BodyTooLarge) => {
                        let _ = stream.write_all(http_parse::RESPONSE_413.to_vec()).await;
                        return;
                    }
                    Err(ParseError::InvalidRequest) => {
                        let _ = stream.write_all(http_parse::RESPONSE_400.to_vec()).await;
                        return;
                    }
                }
            }
            None => {
                // No pool or arena full — use Vec path for this read cycle.
                accum = Some((vec![0u8; DEFAULT_BUF_SIZE], 0));
            }
        }
    }
}

/// Execute a parsed request and write the response. Returns `true` to continue
/// the connection, `false` to close it.
async fn execute_and_respond(
    stream: &MahaloStream,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    runtime: &Rc<Runtime>,
    ws_config: &Option<WsConfig>,
    parsed: http_parse::ParsedRequest,
    resp_buf: &mut Vec<u8>,
) -> bool {
    let keep_alive = parsed.keep_alive;

    // Check for WebSocket upgrade.
    if let (Some(ws_key), Some(_wsc)) = (&parsed.ws_key, ws_config.as_ref()) {
        resp_buf.clear();
        http_parse::serialize_ws_accept_response(ws_key, resp_buf);
        let resp = std::mem::take(resp_buf);
        let BufResult(result, returned) = stream.write_all(resp).await;
        *resp_buf = returned;
        if result.is_err() {
            return false;
        }
        tracing::warn!("WebSocket upgrade accepted but handler not yet implemented");
        return false;
    }

    let conn = if ws_config.is_some() {
        parsed.conn.with_runtime(Rc::clone(runtime))
    } else {
        parsed.conn
    };

    let mut conn = crate::handler::execute_request(conn, router, error_handler, after_plugs).await;

    // Check for SSE stream assign — if present, switch to streaming mode.
    if let Some(sse_stream) = conn.take_assign::<SseStreamKey>() {
        let keep_alive_cfg = conn.take_assign::<SseKeepAliveKey>();

        resp_buf.clear();
        http_parse::serialize_sse_headers_into(&conn, resp_buf);
        let resp = std::mem::take(resp_buf);
        let BufResult(result, returned) = stream.write_all(resp).await;
        *resp_buf = returned;
        if result.is_err() {
            return false;
        }

        sse_streaming_loop(stream, sse_stream, keep_alive_cfg).await;
        return false; // SSE connection done — no keep-alive
    }

    resp_buf.clear();
    http_parse::serialize_response_into(&conn, keep_alive, resp_buf);

    let resp = std::mem::take(resp_buf);
    let BufResult(result, returned) = stream.write_all(resp).await;
    *resp_buf = returned;
    if result.is_err() {
        return false;
    }

    keep_alive
}

/// Stream SSE events from `sse_stream.rx` to the stream.
///
/// If keep-alive is configured, sends a comment when idle for the keep-alive interval.
/// Exits when the sender is dropped (rx returns None) or on write error.
async fn sse_streaming_loop(
    stream: &MahaloStream,
    mut sse_stream: mahalo_core::conn::SseStream,
    keep_alive_cfg: Option<mahalo_core::conn::KeepAlive>,
) {
    match keep_alive_cfg {
        Some(ka) => {
            let mut comment_bytes = format!(": {}\n\n", ka.text).into_bytes();
            loop {
                match rebar_core::time::timeout(ka.interval, sse_stream.rx.recv()).await {
                    Ok(Some(data)) => {
                        let BufResult(result, _) = stream.write_all(data.into_bytes()).await;
                        if result.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return, // sender dropped
                    Err(_elapsed) => {
                        let BufResult(result, returned) =
                            stream.write_all(comment_bytes).await;
                        comment_bytes = returned;
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }
        }
        None => {
            while let Some(data) = sse_stream.rx.recv().await {
                let BufResult(result, _) = stream.write_all(data.into_bytes()).await;
                if result.is_err() {
                    return;
                }
            }
        }
    }
}

/// Vec-based read + parse loop (fallback path). Returns `true` to continue
/// the connection (caller should check if accum is drained), `false` to close.
async fn vec_read_loop(
    stream: &MahaloStream,
    peer_addr: SocketAddr,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    runtime: &Rc<Runtime>,
    body_limit: usize,
    ws_config: &Option<WsConfig>,
    buf: &mut Vec<u8>,
    filled: &mut usize,
    resp_buf: &mut Vec<u8>,
) -> bool {
    // Read more data.
    if *filled == buf.len() {
        if buf.len() >= body_limit + DEFAULT_BUF_SIZE {
            let _ = stream.write_all(http_parse::RESPONSE_413.to_vec()).await;
            return false;
        }
        buf.resize(buf.len() * 2, 0);
    }

    let read_buf = buf.split_off(*filled);
    let BufResult(result, returned_buf) = stream.read(read_buf).await;
    match result {
        Ok(0) => return false,
        Ok(n) => {
            buf.extend_from_slice(&returned_buf[..n]);
            *filled += n;
        }
        Err(_) => return false,
    }

    // Parse loop — process as many complete requests as possible.
    loop {
        match http_parse::try_parse_request(&buf[..*filled], body_limit, peer_addr) {
            Ok(Some(parsed)) => {
                let bytes_consumed = parsed.bytes_consumed;

                if !execute_and_respond(
                    stream, router, error_handler, after_plugs,
                    runtime, ws_config, parsed, resp_buf,
                )
                .await
                {
                    return false;
                }

                // Shift unconsumed bytes to the front.
                buf.copy_within(bytes_consumed..*filled, 0);
                *filled -= bytes_consumed;

                if *filled == 0 {
                    return true;
                }
            }
            Ok(None) => {
                if *filled == buf.len() {
                    if buf.len() >= body_limit + DEFAULT_BUF_SIZE {
                        let _ = stream.write_all(http_parse::RESPONSE_413.to_vec()).await;
                        return false;
                    }
                    buf.resize(buf.len() * 2, 0);
                }
                return true;
            }
            Err(ParseError::BodyTooLarge) => {
                let _ = stream.write_all(http_parse::RESPONSE_413.to_vec()).await;
                return false;
            }
            Err(ParseError::InvalidRequest) => {
                let _ = stream.write_all(http_parse::RESPONSE_400.to_vec()).await;
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use mahalo_core::conn::Conn;
    use mahalo_core::plug::plug_fn;
    use rebar_core::executor::{ExecutorConfig, RebarExecutor};
    use std::io::{Read as _, Write as _};
    use turbine_core::config::PoolConfig;

    /// Helper: send an HTTP request to `addr` on a background std thread
    /// and return the raw response bytes.
    fn http_roundtrip(addr: std::net::SocketAddr, request: &[u8]) -> String {
        let request = request.to_vec();
        let handle = std::thread::spawn(move || {
            // Small delay to let the accept loop start.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            stream.write_all(&request).unwrap();
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            String::from_utf8(response).unwrap()
        });
        handle.join().unwrap()
    }

    /// Run the accept loop with the given executor config and router,
    /// send one request, and return the response.
    fn serve_one_request(
        exec_config: ExecutorConfig,
        route: &str,
        body: &str,
        request: &[u8],
    ) -> String {
        let route = route.to_string();
        let body = body.to_string();
        let request = request.to_vec();

        // We need the address before we can send the request, but the
        // accept loop runs inside block_on. Use a channel to pass it out.
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(exec_config).unwrap();
            ex.block_on(async {
                let listener =
                    rebar_core::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();

                let router = MahaloRouter::new().get(
                    &route,
                    plug_fn(move |conn: Conn| {
                        let body = body.clone();
                        async move { conn.put_status(StatusCode::OK).put_resp_body(body) }
                    }),
                );
                let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));

                run_accept_loop(listener, router, None, vec![], runtime, 2 * 1024 * 1024, None, None)
                    .await;
            });
        });

        let addr = addr_rx.recv().unwrap();
        let response = http_roundtrip(addr, &request);
        drop(handle); // Server thread runs forever; test just checks the response.
        response
    }

    #[test]
    fn lease_read_path_serves_http_request() {
        let response = serve_one_request(
            ExecutorConfig {
                pool_config: Some(PoolConfig::default()),
                ..Default::default()
            },
            "/hello",
            "world",
            b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected response: {response}"
        );
        assert!(
            response.contains("world"),
            "response body missing: {response}"
        );
    }

    #[test]
    fn sse_streams_events_and_closes_on_sender_drop() {
        let request = b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let listener =
                    rebar_core::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();

                let router = MahaloRouter::new().get(
                    "/events",
                    plug_fn(|conn: Conn| async move {
                        let (tx, rx) = local_sync::mpsc::unbounded::channel::<String>();

                        let conn = conn
                            .put_status(StatusCode::OK)
                            .put_resp_header("content-type", "text/event-stream")
                            .put_resp_header("cache-control", "no-cache")
                            .assign::<mahalo_core::conn::SseStreamKey>(
                                mahalo_core::conn::SseStream { rx },
                            );

                        // Spawn a task that sends 3 events then drops sender.
                        rebar_core::executor::spawn(async move {
                            for i in 1..=3 {
                                let _ = tx.send(format!("data: event {i}\n\n"));
                                rebar_core::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                            // tx dropped here — closes the stream
                        })
                        .detach();

                        conn
                    }),
                );
                let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));

                run_accept_loop(listener, router, None, vec![], runtime, 2 * 1024 * 1024, None, None)
                    .await;
            });
        });

        let addr = addr_rx.recv().unwrap();
        let response = http_roundtrip(addr, request);

        // Verify SSE headers.
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected status: {response}"
        );
        assert!(
            response.contains("content-type: text/event-stream"),
            "missing content-type: {response}"
        );
        assert!(
            response.contains("connection: close"),
            "missing connection: close: {response}"
        );
        // No content-length for streaming.
        assert!(
            !response.contains("content-length"),
            "SSE should not have content-length: {response}"
        );

        // Verify all 3 events arrived.
        assert!(
            response.contains("data: event 1"),
            "missing event 1: {response}"
        );
        assert!(
            response.contains("data: event 2"),
            "missing event 2: {response}"
        );
        assert!(
            response.contains("data: event 3"),
            "missing event 3: {response}"
        );

        drop(handle);
    }

    #[test]
    fn sse_keep_alive_sends_comment_before_event() {
        let request = b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let listener =
                    rebar_core::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();

                let router = MahaloRouter::new().get(
                    "/events",
                    plug_fn(|conn: Conn| async move {
                        let (tx, rx) = local_sync::mpsc::unbounded::channel::<String>();

                        let conn = conn
                            .put_status(StatusCode::OK)
                            .put_resp_header("content-type", "text/event-stream")
                            .put_resp_header("cache-control", "no-cache")
                            .assign::<mahalo_core::conn::SseStreamKey>(
                                mahalo_core::conn::SseStream { rx },
                            )
                            .assign::<mahalo_core::conn::SseKeepAliveKey>(
                                mahalo_core::conn::KeepAlive::new(
                                    std::time::Duration::from_millis(50),
                                ),
                            );

                        // Wait long enough for keep-alive to fire, then send an event.
                        rebar_core::executor::spawn(async move {
                            rebar_core::time::sleep(std::time::Duration::from_millis(200)).await;
                            let _ = tx.send("data: after-keepalive\n\n".to_string());
                            // tx dropped — closes stream
                        })
                        .detach();

                        conn
                    }),
                );
                let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));

                run_accept_loop(listener, router, None, vec![], runtime, 2 * 1024 * 1024, None, None)
                    .await;
            });
        });

        let addr = addr_rx.recv().unwrap();
        let response = http_roundtrip(addr, request);

        // Keep-alive comment should appear before the event data.
        let ka_pos = response.find(": keep-alive\n");
        let event_pos = response.find("data: after-keepalive\n");
        assert!(
            ka_pos.is_some(),
            "keep-alive comment not found: {response}"
        );
        assert!(
            event_pos.is_some(),
            "event data not found: {response}"
        );
        assert!(
            ka_pos.unwrap() < event_pos.unwrap(),
            "keep-alive should appear before the event: {response}"
        );

        drop(handle);
    }

    /// Generate self-signed cert and matching server/client TLS configs.
    fn test_tls_configs() -> (std::sync::Arc<rustls::ServerConfig>, std::sync::Arc<rustls::ClientConfig>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert_der = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
        let key_der =
            rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap();
        let server_config = std::sync::Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .unwrap(),
        );
        let mut root_store = rustls::RootCertStore::empty();
        root_store
            .add(rustls::pki_types::CertificateDer::from(
                cert.cert.der().to_vec(),
            ))
            .unwrap();
        let client_config = std::sync::Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        );
        (server_config, client_config)
    }

    #[test]
    fn tls_serves_https_request() {
        let (server_config, client_config) = test_tls_configs();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Server thread.
        let server_handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let listener =
                    rebar_core::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();

                let router = MahaloRouter::new().get(
                    "/secure",
                    plug_fn(|conn: Conn| async move {
                        conn.put_status(StatusCode::OK).put_resp_body("tls-ok")
                    }),
                );
                let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));
                let tls_acceptor = Some(rebar_core::tls::TlsAcceptor::new(server_config));

                run_accept_loop(
                    listener, router, None, vec![], runtime, 2 * 1024 * 1024, None, tls_acceptor,
                )
                .await;
            });
        });

        // Client thread — uses compio TLS connector for the handshake.
        let addr = addr_rx.recv().unwrap();
        let client_handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let stream = rebar_core::io::TcpStream::connect(addr).await.unwrap();
                let connector = compio_tls::TlsConnector::from(client_config);
                let mut tls = connector.connect("localhost", stream).await.unwrap();

                use compio_io::{AsyncRead, AsyncWrite, AsyncWriteExt};
                let req = b"GET /secure HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_vec();
                AsyncWriteExt::write_all(&mut tls, req).await.0.unwrap();
                AsyncWrite::flush(&mut tls).await.unwrap();

                let mut all_data = Vec::new();
                loop {
                    let buf = Vec::with_capacity(4096);
                    let rebar_core::io::BufResult(result, buf) = AsyncRead::read(&mut tls, buf).await;
                    match result {
                        Ok(0) => break,
                        Ok(n) => all_data.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                result_tx.send(String::from_utf8(all_data).unwrap()).unwrap();
            });
        });

        let response = result_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected response: {response}"
        );
        assert!(
            response.contains("tls-ok"),
            "response body missing: {response}"
        );

        drop(server_handle);
        client_handle.join().unwrap();
    }

    #[test]
    fn tls_sse_streams_events() {
        let (server_config, client_config) = test_tls_configs();

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        let server_handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let listener =
                    rebar_core::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();

                let router = MahaloRouter::new().get(
                    "/events",
                    plug_fn(|conn: Conn| async move {
                        let (tx, rx) = local_sync::mpsc::unbounded::channel::<String>();

                        let conn = conn
                            .put_status(StatusCode::OK)
                            .put_resp_header("content-type", "text/event-stream")
                            .put_resp_header("cache-control", "no-cache")
                            .assign::<mahalo_core::conn::SseStreamKey>(
                                mahalo_core::conn::SseStream { rx },
                            );

                        rebar_core::executor::spawn(async move {
                            for i in 1..=3 {
                                let _ = tx.send(format!("data: tls-event {i}\n\n"));
                                rebar_core::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        })
                        .detach();

                        conn
                    }),
                );
                let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));
                let tls_acceptor = Some(rebar_core::tls::TlsAcceptor::new(server_config));

                run_accept_loop(
                    listener, router, None, vec![], runtime, 2 * 1024 * 1024, None, tls_acceptor,
                )
                .await;
            });
        });

        let addr = addr_rx.recv().unwrap();
        let client_handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let stream = rebar_core::io::TcpStream::connect(addr).await.unwrap();
                let connector = compio_tls::TlsConnector::from(client_config);
                let mut tls = connector.connect("localhost", stream).await.unwrap();

                use compio_io::{AsyncRead, AsyncWrite, AsyncWriteExt};
                let req = b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_vec();
                AsyncWriteExt::write_all(&mut tls, req).await.0.unwrap();
                AsyncWrite::flush(&mut tls).await.unwrap();

                let mut all_data = Vec::new();
                loop {
                    let buf = Vec::with_capacity(4096);
                    let rebar_core::io::BufResult(result, buf) = AsyncRead::read(&mut tls, buf).await;
                    match result {
                        Ok(0) => break,
                        Ok(n) => all_data.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                result_tx.send(String::from_utf8(all_data).unwrap()).unwrap();
            });
        });

        let response = result_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(
            response.contains("text/event-stream"),
            "missing SSE content-type: {response}"
        );
        assert!(
            response.contains("data: tls-event 1"),
            "missing event 1: {response}"
        );
        assert!(
            response.contains("data: tls-event 2"),
            "missing event 2: {response}"
        );
        assert!(
            response.contains("data: tls-event 3"),
            "missing event 3: {response}"
        );

        drop(server_handle);
        client_handle.join().unwrap();
    }

    #[test]
    fn tls_handshake_failure_does_not_crash_server() {
        let (server_config, client_config) = test_tls_configs();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        // Server thread with TLS enabled.
        let server_handle = std::thread::spawn(move || {
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let listener =
                    rebar_core::io::TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();

                let router = MahaloRouter::new().get(
                    "/ok",
                    plug_fn(|conn: Conn| async move {
                        conn.put_status(StatusCode::OK).put_resp_body("still-alive")
                    }),
                );
                let runtime = Rc::new(rebar_core::runtime::Runtime::new(1));
                let tls_acceptor = Some(rebar_core::tls::TlsAcceptor::new(server_config));

                run_accept_loop(
                    listener, router, None, vec![], runtime, 2 * 1024 * 1024, None, tls_acceptor,
                )
                .await;
            });
        });

        let addr = addr_rx.recv().unwrap();

        // 1) Send a plain TCP (non-TLS) request — this should fail the handshake
        //    but NOT crash the server.
        let bad_handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let _ = stream.write_all(b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            // The server should close the connection without a valid HTTP response.
            response
        });
        let bad_response = bad_handle.join().unwrap();
        // Plain TCP to TLS server yields no valid HTTP response (empty or TLS alert).
        assert!(
            bad_response.is_empty() || !bad_response.starts_with(b"HTTP/1.1"),
            "plain TCP should not get a valid HTTP response from TLS server"
        );

        // 2) Now send a proper TLS request — the server should still be alive.
        let good_handle = std::thread::spawn(move || {
            // Small delay to ensure the failed connection is fully cleaned up.
            std::thread::sleep(std::time::Duration::from_millis(100));
            let ex = RebarExecutor::new(ExecutorConfig::default()).unwrap();
            ex.block_on(async {
                let stream = rebar_core::io::TcpStream::connect(addr).await.unwrap();
                let connector = compio_tls::TlsConnector::from(client_config);
                let mut tls = connector.connect("localhost", stream).await.unwrap();

                use compio_io::{AsyncRead, AsyncWrite, AsyncWriteExt};
                let req = b"GET /ok HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_vec();
                AsyncWriteExt::write_all(&mut tls, req).await.0.unwrap();
                AsyncWrite::flush(&mut tls).await.unwrap();

                let mut all_data = Vec::new();
                loop {
                    let buf = Vec::with_capacity(4096);
                    let rebar_core::io::BufResult(result, buf) = AsyncRead::read(&mut tls, buf).await;
                    match result {
                        Ok(0) => break,
                        Ok(n) => all_data.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                result_tx.send(String::from_utf8(all_data).unwrap()).unwrap();
            });
        });

        let response = result_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "server should still be alive after failed handshake: {response}"
        );
        assert!(
            response.contains("still-alive"),
            "response body missing: {response}"
        );

        drop(server_handle);
        good_handle.join().unwrap();
    }

    #[test]
    fn fallback_vec_path_serves_http_request() {
        let response = serve_one_request(
            ExecutorConfig::default(),
            "/ping",
            "pong",
            b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "unexpected response: {response}"
        );
        assert!(
            response.contains("pong"),
            "response body missing: {response}"
        );
    }
}
