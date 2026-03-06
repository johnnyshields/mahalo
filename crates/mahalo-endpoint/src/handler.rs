use std::net::SocketAddr;

use http::StatusCode;
use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use socket2::{Domain, Protocol, Socket, Type};

use crate::endpoint::ErrorHandler;

/// Create and bind a TCP socket with optional `SO_REUSEPORT`.
///
/// Shared by both the io_uring worker (Linux) and the tokio TCP server (other platforms).
pub(crate) fn bind_socket(
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

/// Execute a request through the router and after-plugs.
///
/// Shared by both the io_uring event loop (Linux) and the tokio TCP server (other platforms).
#[inline]
pub async fn execute_request(
    conn: Conn,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
) -> Conn {
    let resolved = router.resolve(&conn.method, conn.uri.path());

    let mut conn = match resolved {
        Some(resolved) => resolved.execute(conn).await,
        None => {
            if let Some(handler) = error_handler {
                let conn = conn.put_status(StatusCode::NOT_FOUND);
                handler(StatusCode::NOT_FOUND, conn)
            } else {
                conn.put_status(StatusCode::NOT_FOUND)
                    .put_resp_body("Not Found")
            }
        }
    };

    for plug in after_plugs {
        if conn.halted {
            break;
        }
        conn = plug.call(conn).await;
    }
    conn
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::Method;
    use mahalo_core::plug::plug_fn;
    use std::sync::Arc;

    fn ok_router() -> MahaloRouter {
        MahaloRouter::new().get(
            "/hello",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK).put_resp_body("world")
            }),
        )
    }

    #[tokio::test]
    async fn execute_request_route_match() {
        let router = ok_router();
        let conn = Conn::new(Method::GET, http::Uri::from_static("/hello"));
        let result = execute_request(conn, &router, &None, &[]).await;

        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.resp_body, Bytes::from("world"));
    }

    #[tokio::test]
    async fn execute_request_default_404() {
        let router = ok_router();
        let conn = Conn::new(Method::GET, http::Uri::from_static("/missing"));
        let result = execute_request(conn, &router, &None, &[]).await;

        assert_eq!(result.status, StatusCode::NOT_FOUND);
        assert_eq!(result.resp_body, Bytes::from("Not Found"));
    }

    #[tokio::test]
    async fn execute_request_custom_error_handler() {
        let router = ok_router();
        let handler: ErrorHandler = Arc::new(|status, conn| {
            conn.put_resp_body(format!("Custom {}", status.as_u16()))
        });
        let conn = Conn::new(Method::GET, http::Uri::from_static("/missing"));
        let result = execute_request(conn, &router, &Some(handler), &[]).await;

        assert_eq!(result.status, StatusCode::NOT_FOUND);
        assert_eq!(result.resp_body, Bytes::from("Custom 404"));
    }

    #[tokio::test]
    async fn execute_request_runs_after_plugs() {
        let router = ok_router();
        let after: Vec<Box<dyn Plug>> = vec![Box::new(plug_fn(|conn: Conn| async {
            conn.put_resp_header("x-after", "yes")
        }))];
        let conn = Conn::new(Method::GET, http::Uri::from_static("/hello"));
        let result = execute_request(conn, &router, &None, &after).await;

        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(result.get_resp_header("x-after").unwrap(), "yes");
    }

    #[tokio::test]
    async fn execute_request_halt_stops_after_plugs() {
        let router = MahaloRouter::new().get(
            "/halt",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_body("halted")
                    .halt()
            }),
        );
        let after: Vec<Box<dyn Plug>> = vec![Box::new(plug_fn(|conn: Conn| async {
            conn.put_resp_header("x-should-not-run", "true")
        }))];
        let conn = Conn::new(Method::GET, http::Uri::from_static("/halt"));
        let result = execute_request(conn, &router, &None, &after).await;

        assert_eq!(result.resp_body, Bytes::from("halted"));
        assert!(result.get_resp_header("x-should-not-run").is_none());
    }

    #[test]
    fn bind_socket_ipv4() {
        let socket = bind_socket("127.0.0.1:0".parse().unwrap(), false).unwrap();
        let local_addr = socket.local_addr().unwrap().as_socket().unwrap();
        assert_ne!(local_addr.port(), 0);
    }

    #[test]
    fn bind_socket_with_reuse_port() {
        let socket = bind_socket("127.0.0.1:0".parse().unwrap(), true).unwrap();
        let local_addr = socket.local_addr().unwrap().as_socket().unwrap();
        assert_ne!(local_addr.port(), 0);
    }
}
