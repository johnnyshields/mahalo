use std::net::SocketAddr;
use std::sync::Arc;

use mahalo_channel::socket::ChannelRouter;
use mahalo_core::plug::Plug;
use mahalo_pubsub::PubSub;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::endpoint::{ErrorHandler, WsConfig};
use crate::handler::bind_socket;
use crate::http_parse::{self, ParseError};

/// Start a multi-threaded tokio-based TCP server.
///
/// Spawns one worker thread per available CPU core, each running a
/// single-threaded tokio runtime. On platforms that support `SO_REUSEPORT`
/// (Linux, macOS) each worker binds its own listener; on Windows a single
/// `TcpListener` is shared via `Arc`.
pub fn start_tcp_server(
    addr: SocketAddr,
    router: Arc<MahaloRouter>,
    error_handler: Option<ErrorHandler>,
    after_plugs: Arc<Vec<Box<dyn Plug>>>,
    runtime: Arc<Runtime>,
    body_limit: usize,
    ws_config: Option<WsConfig>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let error_handler = Arc::new(error_handler);

    // On Windows, SO_REUSEPORT is not available — share a single listener.
    #[cfg(target_os = "windows")]
    let shared_listener = {
        let socket = bind_socket(addr, false)?;
        let std_listener: std::net::TcpListener = socket.into();
        std_listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(std_listener)?;
        Some(Arc::new(listener))
    };

    let mut handles = Vec::with_capacity(num_workers);

    for i in 0..num_workers {
        let router = Arc::clone(&router);
        let error_handler = Arc::clone(&error_handler);
        let after_plugs = Arc::clone(&after_plugs);
        let runtime = Arc::clone(&runtime);
        let ws_config = ws_config.clone();

        #[cfg(target_os = "windows")]
        let shared_listener = shared_listener.clone();

        let handle = std::thread::Builder::new()
            .name(format!("tcp-worker-{i}"))
            .spawn(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let tokio_rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;

                tokio_rt.block_on(async move {
                    // Per-worker listener on platforms with SO_REUSEPORT.
                    #[cfg(not(target_os = "windows"))]
                    let listener = {
                        let socket = bind_socket(addr, true)?;
                        let std_listener: std::net::TcpListener = socket.into();
                        std_listener.set_nonblocking(true)?;
                        TcpListener::from_std(std_listener)?
                    };

                    #[cfg(target_os = "windows")]
                    let listener_ref = shared_listener.as_ref().unwrap();

                    loop {
                        #[cfg(not(target_os = "windows"))]
                        let accept_result = listener.accept().await;
                        #[cfg(target_os = "windows")]
                        let accept_result = listener_ref.accept().await;

                        let (stream, peer_addr) = match accept_result {
                            Ok(conn) => conn,
                            Err(e) => {
                                tracing::warn!("accept error: {}", e);
                                continue;
                            }
                        };

                        // Set TCP_NODELAY via socket2.
                        let _ = stream.set_nodelay(true);

                        let router = Arc::clone(&router);
                        let error_handler = Arc::clone(&error_handler);
                        let after_plugs = Arc::clone(&after_plugs);
                        let runtime = Arc::clone(&runtime);
                        let ws_config = ws_config.clone();

                        tokio::spawn(async move {
                            handle_connection(
                                stream,
                                peer_addr,
                                &router,
                                &error_handler,
                                &after_plugs,
                                &runtime,
                                body_limit,
                                ws_config.as_ref(),
                            )
                            .await;
                        });
                    }

                    #[allow(unreachable_code)]
                    Ok(())
                })
            })?;

        handles.push(handle);
    }

    for handle in handles {
        handle
            .join()
            .map_err(|e| format!("worker thread panicked: {:?}", e))??;
    }

    Ok(())
}

/// Handle a single TCP connection, supporting keep-alive and WebSocket upgrade.
async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    peer_addr: SocketAddr,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    runtime: &Arc<Runtime>,
    body_limit: usize,
    ws_config: Option<&WsConfig>,
) {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0;
    let mut resp_buf = Vec::with_capacity(256);

    loop {
        // Read data from the stream.
        match stream.read(&mut buf[filled..]).await {
            Ok(0) => return, // Connection closed.
            Ok(n) => filled += n,
            Err(_) => return,
        }

        // Try to parse a complete request.
        loop {
            match http_parse::try_parse_request(&buf[..filled], body_limit, peer_addr) {
                Ok(Some(parsed)) => {
                    // Check for WebSocket upgrade.
                    if let (Some(ws_key), Some(wsc)) =
                        (parsed.ws_key, ws_config)
                    {
                        // Write the 101 response manually.
                        resp_buf.clear();
                        http_parse::serialize_ws_accept_response(&ws_key, &mut resp_buf);
                        if stream.write_all(&resp_buf).await.is_err() {
                            return;
                        }

                        // Upgrade the raw TCP stream to a WebSocket using tokio-tungstenite.
                        handle_ws_upgraded(stream, &wsc.channel_router, &wsc.pubsub, runtime).await;
                        return;
                    }

                    let keep_alive = parsed.keep_alive;
                    let bytes_consumed = parsed.bytes_consumed;

                    let conn = parsed.conn.with_runtime(Arc::clone(runtime));
                    let conn = crate::handler::execute_request(
                        conn,
                        router,
                        error_handler,
                        after_plugs,
                    )
                    .await;

                    http_parse::serialize_response_into(&conn, keep_alive, &mut resp_buf);
                    if stream.write_all(&resp_buf).await.is_err() {
                        return;
                    }

                    // Shift unconsumed bytes to the front.
                    buf.copy_within(bytes_consumed..filled, 0);
                    filled -= bytes_consumed;

                    if !keep_alive {
                        return;
                    }

                    // If there's remaining data, try to parse another request.
                    if filled == 0 {
                        break;
                    }
                }
                Ok(None) => {
                    // Need more data — grow buffer if full.
                    if filled == buf.len() {
                        if buf.len() >= body_limit + 8192 {
                            // Request too large.
                            let _ = stream.write_all(http_parse::RESPONSE_413).await;
                            return;
                        }
                        buf.resize(buf.len() * 2, 0);
                    }
                    break;
                }
                Err(ParseError::BodyTooLarge) => {
                    let _ = stream.write_all(http_parse::RESPONSE_413).await;
                    return;
                }
                Err(ParseError::InvalidRequest) => {
                    let _ = stream.write_all(http_parse::RESPONSE_400).await;
                    return;
                }
            }
        }
    }
}

/// Handle an upgraded WebSocket connection on the tokio TCP path.
///
/// Uses tokio-tungstenite to wrap the raw TcpStream, then bridges to
/// the mahalo-channel GenServer via the same mpsc-based ChannelSocket.
async fn handle_ws_upgraded(
    stream: tokio::net::TcpStream,
    channel_router: &Arc<ChannelRouter>,
    pubsub: &PubSub,
    runtime: &Arc<Runtime>,
) {
    use futures::{SinkExt, StreamExt};
    use rebar_core::gen_server;
    use tokio::sync::mpsc;
    use tokio_tungstenite::WebSocketStream;
    use tungstenite::protocol::Role;

    let ws_stream = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<mahalo_channel::WsSendItem>();

    // Start GenServer for this connection
    let server = mahalo_channel::ChannelConnectionServer::new(
        Arc::clone(channel_router),
        pubsub.clone(),
        tx,
        Arc::clone(runtime),
    );
    let pid = gen_server::start(runtime, server, rmpv::Value::Nil).await;

    // Spawn forwarder: mpsc String → tungstenite Text frame
    let send_task = tokio::spawn(async move {
        while let Some(json) = rx.recv().await {
            if ws_sender
                .send(tungstenite::Message::Text(json.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // Read loop: tungstenite → GenServer cast
    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            tungstenite::Message::Text(ref text) => {
                let cast_val = rmpv::Value::String(rmpv::Utf8String::from(text.to_string()));
                if gen_server::cast_from_runtime(runtime, pid, cast_val)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            tungstenite::Message::Close(_) => break,
            _ => {}
        }
    }

    // Kill the GenServer — this triggers terminate() for cleanup (unsubscribe, leave channels).
    // Killing also drops internal tx clones, causing the send_task's rx.recv() to return None.
    runtime.kill(pid);
    // Give the send_task a brief window to finish gracefully before aborting.
    tokio::select! {
        _ = send_task => {}
        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use mahalo_core::conn::Conn;
    use mahalo_core::plug::plug_fn;
    use rebar_core::runtime::Runtime;
    use tokio::net::TcpListener;

    #[test]
    fn bind_socket_creates_listener() {
        let socket = crate::handler::bind_socket("127.0.0.1:0".parse().unwrap(), false).unwrap();
        let local_addr = socket.local_addr().unwrap().as_socket().unwrap();
        assert_ne!(local_addr.port(), 0);
    }

    /// Helper: spawn `handle_connection` on one side of a TCP pair, return the client side.
    async fn setup_conn(
        router: mahalo_router::MahaloRouter,
        body_limit: usize,
    ) -> tokio::net::TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let router = Arc::new(router);
        let error_handler: Option<ErrorHandler> = None;
        let after_plugs: Arc<Vec<Box<dyn mahalo_core::plug::Plug>>> = Arc::new(vec![]);
        let runtime = Arc::new(Runtime::new(1));

        tokio::spawn(async move {
            let (stream, peer_addr) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_addr,
                &router,
                &error_handler,
                &after_plugs,
                &runtime,
                body_limit,
                None,
            )
            .await;
        });

        tokio::net::TcpStream::connect(addr).await.unwrap()
    }

    fn hello_router() -> mahalo_router::MahaloRouter {
        mahalo_router::MahaloRouter::new().get(
            "/hello",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK).put_resp_body("world")
            }),
        )
    }

    #[tokio::test]
    async fn handle_connection_basic_request() {
        let mut client = setup_conn(hello_router(), 1024 * 1024).await;

        client
            .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();

        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.ends_with("world"));
    }

    #[tokio::test]
    async fn handle_connection_keep_alive_pipelining() {
        let mut client = setup_conn(hello_router(), 1024 * 1024).await;

        // Send two requests back-to-back on the same connection.
        client
            .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\nGET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();

        // Should see two responses.
        let count = resp.matches("HTTP/1.1 200 OK").count();
        assert_eq!(count, 2, "expected 2 responses, got:\n{resp}");
    }

    #[tokio::test]
    async fn handle_connection_404_for_unknown_route() {
        let mut client = setup_conn(hello_router(), 1024 * 1024).await;

        client
            .write_all(b"GET /nope HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();

        assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[tokio::test]
    async fn handle_connection_400_on_invalid_request() {
        let mut client = setup_conn(hello_router(), 1024 * 1024).await;

        client.write_all(b"NOT A VALID HTTP REQUEST\r\n\r\n").await.unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();

        assert_eq!(&resp, http_parse::RESPONSE_400);
    }

    #[tokio::test]
    async fn handle_connection_413_on_body_too_large() {
        let mut client = setup_conn(hello_router(), 16).await; // tiny body limit

        client
            .write_all(b"POST /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n\r\n")
            .await
            .unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();

        assert_eq!(&resp, http_parse::RESPONSE_413);
    }

    #[tokio::test]
    async fn handle_connection_closes_on_connection_close() {
        let mut client = setup_conn(hello_router(), 1024 * 1024).await;

        client
            .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();

        assert!(resp.contains("connection: close\r\n"));
    }
}
