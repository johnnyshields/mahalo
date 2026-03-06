use std::net::SocketAddr;
use std::sync::Arc;

use mahalo_channel::socket::ChannelRouter;
use mahalo_core::plug::Plug;
use mahalo_pubsub::PubSub;
use mahalo_router::MahaloRouter;
use rebar_core::runtime::Runtime;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::endpoint::ErrorHandler;
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
    channel_router: Option<Arc<ChannelRouter>>,
    pubsub: Option<PubSub>,
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
        let channel_router = channel_router.clone();
        let pubsub = pubsub.clone();

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
                        let channel_router = channel_router.clone();
                        let pubsub = pubsub.clone();

                        tokio::spawn(async move {
                            handle_connection(
                                stream,
                                peer_addr,
                                &router,
                                &error_handler,
                                &after_plugs,
                                &runtime,
                                body_limit,
                                channel_router.as_ref(),
                                pubsub.as_ref(),
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

/// Create and bind a TCP socket with optional SO_REUSEPORT.
fn bind_socket(
    addr: SocketAddr,
    reuse_port: bool,
) -> Result<Socket, Box<dyn std::error::Error + Send + Sync>> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    if reuse_port {
        #[cfg(not(target_os = "windows"))]
        socket.set_reuse_port(true)?;
    }
    socket.set_nodelay(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(8192)?;
    Ok(socket)
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
    channel_router: Option<&Arc<ChannelRouter>>,
    pubsub: Option<&PubSub>,
) {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0;

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
                    if let (Some(ws_key), Some(cr), Some(ps)) =
                        (parsed.ws_key, channel_router, pubsub)
                    {
                        // Write the 101 response manually.
                        let mut resp_buf = Vec::new();
                        http_parse::serialize_ws_accept_response(&ws_key, &mut resp_buf);
                        if stream.write_all(&resp_buf).await.is_err() {
                            return;
                        }

                        // Upgrade the raw TCP stream to a WebSocket using tokio-tungstenite.
                        handle_ws_upgraded(stream, cr, ps, runtime).await;
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

                    let response = http_parse::serialize_response(&conn, keep_alive);
                    if stream.write_all(&response).await.is_err() {
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

    runtime.kill(pid);
    send_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_socket_creates_listener() {
        let socket = bind_socket("127.0.0.1:0".parse().unwrap(), false).unwrap();
        let local_addr = socket.local_addr().unwrap().as_socket().unwrap();
        assert_ne!(local_addr.port(), 0);
    }
}
