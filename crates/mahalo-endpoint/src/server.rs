use std::net::SocketAddr;
use std::rc::Rc;

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::TcpListener;

use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;

use crate::endpoint::{ErrorHandler, WsConfig};
use crate::http_parse::{self, ParseError};

/// Run the accept loop on the current monoio runtime using an existing listener.
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

                monoio::spawn(async move {
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
                });
            }
            Err(e) => {
                tracing::warn!("accept error: {e}");
            }
        }
    }
}

/// Handle a single TCP connection, supporting HTTP keep-alive and WebSocket upgrade.
async fn handle_connection(
    mut stream: monoio::net::TcpStream,
    peer_addr: SocketAddr,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    runtime: &Rc<Runtime>,
    body_limit: usize,
    ws_config: &Option<WsConfig>,
) {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0usize;
    let mut resp_buf = Vec::with_capacity(256);

    loop {
        // Ensure there's room to read into.
        if filled == buf.len() {
            if buf.len() >= body_limit + 8192 {
                let _ = stream.write_all(http_parse::RESPONSE_413.to_vec()).await;
                return;
            }
            buf.resize(buf.len() * 2, 0);
        }

        // monoio ownership-based read: split off the unfilled portion.
        let read_buf = buf.split_off(filled);
        let (result, returned_buf) = stream.read(read_buf).await;
        match result {
            Ok(0) => return, // EOF
            Ok(n) => {
                // Rejoin: buf contains [0..filled], returned_buf has the new data.
                buf.extend_from_slice(&returned_buf[..n]);
                filled += n;
            }
            Err(_) => return,
        }

        // Parse loop — process as many complete requests as possible.
        loop {
            match http_parse::try_parse_request(&buf[..filled], body_limit, peer_addr) {
                Ok(Some(parsed)) => {
                    let keep_alive = parsed.keep_alive;
                    let bytes_consumed = parsed.bytes_consumed;

                    // Check for WebSocket upgrade.
                    if let (Some(ws_key), Some(_wsc)) = (&parsed.ws_key, ws_config.as_ref()) {
                        resp_buf.clear();
                        http_parse::serialize_ws_accept_response(ws_key, &mut resp_buf);
                        let (result, _) = stream.write_all(resp_buf.clone()).await;
                        if result.is_err() {
                            return;
                        }

                        // TODO(phase4): WebSocket handling requires mahalo-channel
                        // adaptation to monoio. For now, accept the upgrade but
                        // close the connection immediately.
                        tracing::warn!("WebSocket upgrade accepted but handler not yet implemented for monoio");
                        return;
                    }

                    let conn = if ws_config.is_some() {
                        parsed.conn.with_runtime(Rc::clone(runtime))
                    } else {
                        parsed.conn
                    };

                    let conn = crate::handler::execute_request(
                        conn,
                        router,
                        error_handler,
                        after_plugs,
                    )
                    .await;

                    resp_buf.clear();
                    http_parse::serialize_response_into(&conn, keep_alive, &mut resp_buf);

                    // monoio write — takes ownership of buffer.
                    let (result, _) = stream.write_all(resp_buf.clone()).await;
                    if result.is_err() {
                        return;
                    }

                    // Shift unconsumed bytes to the front.
                    buf.copy_within(bytes_consumed..filled, 0);
                    filled -= bytes_consumed;

                    if !keep_alive {
                        return;
                    }

                    if filled == 0 {
                        break;
                    }
                }
                Ok(None) => {
                    // Need more data — grow buffer if full.
                    if filled == buf.len() {
                        if buf.len() >= body_limit + 8192 {
                            let _ = stream.write_all(http_parse::RESPONSE_413.to_vec()).await;
                            return;
                        }
                        buf.resize(buf.len() * 2, 0);
                    }
                    break;
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
    }
}
