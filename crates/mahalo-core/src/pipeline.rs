use crate::conn::Conn;
use crate::plug::Plug;

/// An ordered sequence of plugs that execute in order, halting early if
/// `conn.halted` is set.
pub struct Pipeline {
    pub name: String,
    plugs: Vec<Box<dyn Plug>>,
}

impl Pipeline {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            plugs: Vec::new(),
        }
    }

    pub fn plug(mut self, p: impl Plug) -> Self {
        self.plugs.push(Box::new(p));
        self
    }

    #[inline]
    pub async fn execute(&self, mut conn: Conn) -> Conn {
        for plug in &self.plugs {
            if conn.halted {
                break;
            }
            conn = plug.call(conn).await;
        }
        conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::AssignKey;
    use crate::plug::plug_fn;
    use http::{Method, StatusCode, Uri};

    struct CallOrder;
    impl AssignKey for CallOrder {
        type Value = Vec<&'static str>;
    }

    #[tokio::test]
    async fn executes_plugs_in_order() {
        let pipeline = Pipeline::new("test")
            .plug(plug_fn(|conn: Conn| async {
                let mut order = conn.get_assign::<CallOrder>().cloned().unwrap_or_default();
                order.push("first");
                conn.assign::<CallOrder>(order)
            }))
            .plug(plug_fn(|conn: Conn| async {
                let mut order = conn.get_assign::<CallOrder>().cloned().unwrap_or_default();
                order.push("second");
                conn.assign::<CallOrder>(order)
            }));

        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = pipeline.execute(conn).await;

        let order = conn.get_assign::<CallOrder>().unwrap();
        assert_eq!(order, &vec!["first", "second"]);
    }

    #[tokio::test]
    async fn halted_conn_stops_pipeline() {
        let pipeline = Pipeline::new("test")
            .plug(plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::UNAUTHORIZED).halt()
            }))
            .plug(plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
            }));

        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = pipeline.execute(conn).await;

        assert!(conn.halted);
        assert_eq!(conn.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_pipeline_passes_through() {
        let pipeline = Pipeline::new("empty");
        let conn = Conn::new(Method::GET, Uri::from_static("/"));
        let conn = pipeline.execute(conn).await;
        assert_eq!(conn.status, StatusCode::OK);
    }
}
