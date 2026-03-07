use std::net::SocketAddr;
use std::rc::Rc;

use rebar_core::io::{BufResult, TcpListener, TcpStream};

use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;

use crate::endpoint::{ErrorHandler, WsConfig};
use crate::http_parse::{self, ParseError};

/// Run the accept loop on the current executor using an existing listener.
pub(crate) async fn run_accept_loop(
    listener: TcpListener,
    router: MahaloRouter,
    error_handler: Option<ErrorHandler>,
    after_plugs: Vec<Box<dyn Plug>>,
    runtime: Rc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
) {
    let router = Rc::new(router);
    let error_handler = Rc::new(error_handler);
    let after_plugs: Rc<Vec<Box<dyn Plug>>> = Rc::new(after_plugs);
    let ws_config = Rc::new(ws_config);

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let router = Rc::clone(&router);
                let error_handler = Rc::clone(&error_handler);
                let after_plugs = Rc::clone(&after_plugs);
                let runtime = Rc::clone(&runtime);
                let ws_config = Rc::clone(&ws_config);

                rebar_core::executor::spawn(async move {
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

/// Handle a single TCP connection, supporting HTTP keep-alive and WebSocket upgrade.
///
/// Uses a hybrid read strategy:
/// - **Fast path**: When a turbine buffer pool is available, lease an arena buffer
///   for reads. If the request completes in a single read, no allocator call is
///   needed for the read buffer.
/// - **Fallback path**: When no pool is configured, the pool is exhausted, or a
///   partial request spans multiple reads, falls back to a heap-allocated `Vec<u8>`.
async fn handle_connection(
    stream: TcpStream,
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

        // Fast path: lease a buffer, read, try to parse in one shot.
        let lease = rebar_core::executor::with_buffer_pool(|pool| pool.lease(8192)).flatten();

        match lease {
            Some(lease) => {
                let BufResult(result, lease) = stream.read_lease(lease).await;
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

                        if let Some(leftover_bytes) = leftover_data {
                            // Pipelining: copy leftover to accum for next iteration.
                            let mut buf = vec![0u8; 8192];
                            buf[..leftover_bytes.len()].copy_from_slice(&leftover_bytes);
                            accum = Some((buf, leftover_bytes.len()));
                        }
                        // lease drops here — zero-alloc for the common case.
                    }
                    Ok(None) => {
                        // Incomplete — copy to accum, fall back to Vec path.
                        let cap = 8192usize.max(n * 2);
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
                accum = Some((vec![0u8; 8192], 0));
            }
        }
    }
}

/// Execute a parsed request and write the response. Returns `true` to continue
/// the connection, `false` to close it.
async fn execute_and_respond(
    stream: &TcpStream,
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

    let conn = crate::handler::execute_request(conn, router, error_handler, after_plugs).await;

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

/// Vec-based read + parse loop (fallback path). Returns `true` to continue
/// the connection (caller should check if accum is drained), `false` to close.
async fn vec_read_loop(
    stream: &TcpStream,
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
        if buf.len() >= body_limit + 8192 {
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
                    if buf.len() >= body_limit + 8192 {
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
