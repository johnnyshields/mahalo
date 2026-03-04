use crate::conn::Conn;
use crate::plug::BoxFuture;
use http::StatusCode;

/// RESTful controller trait with default 404 implementations for each action.
pub trait Controller: Send + Sync + 'static {
    fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async { conn.put_status(StatusCode::NOT_FOUND) })
    }

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async { conn.put_status(StatusCode::NOT_FOUND) })
    }

    fn create(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async { conn.put_status(StatusCode::NOT_FOUND) })
    }

    fn update(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async { conn.put_status(StatusCode::NOT_FOUND) })
    }

    fn delete(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async { conn.put_status(StatusCode::NOT_FOUND) })
    }

    fn call_action<'a>(&'a self, action: &str, conn: Conn) -> BoxFuture<'a, Conn> {
        match action {
            "index" => self.index(conn),
            "show" => self.show(conn),
            "create" => self.create(conn),
            "update" => self.update(conn),
            "delete" => self.delete(conn),
            _ => Box::pin(async { conn.put_status(StatusCode::NOT_FOUND) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Method, Uri};

    struct TestController;

    impl Controller for TestController {
        fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {
            Box::pin(async {
                conn.put_status(StatusCode::OK)
                    .put_resp_body("index response")
            })
        }
    }

    #[tokio::test]
    async fn overridden_action() {
        let ctrl = TestController;
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = ctrl.call_action("index", conn).await;
        assert_eq!(conn.status, StatusCode::OK);
        assert_eq!(conn.resp_body, "index response");
    }

    #[tokio::test]
    async fn default_action_returns_404() {
        let ctrl = TestController;
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = ctrl.call_action("show", conn).await;
        assert_eq!(conn.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_action_returns_404() {
        let ctrl = TestController;
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = ctrl.call_action("nonexistent", conn).await;
        assert_eq!(conn.status, StatusCode::NOT_FOUND);
    }
}
