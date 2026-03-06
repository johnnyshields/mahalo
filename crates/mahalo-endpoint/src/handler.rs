use http::StatusCode;
use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;

use crate::endpoint::ErrorHandler;

/// Execute a request through the router and after-plugs.
///
/// Shared by both the io_uring event loop (Linux) and the tokio TCP server (other platforms).
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
