pub fn controller_file(pascal: &str) -> String {
    format!(
        r#"use mahalo::{{BoxFuture, Conn, Controller}};
use http::StatusCode;

pub struct {pascal}Controller;

impl Controller for {pascal}Controller {{
    fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {{
        Box::pin(async move {{
            conn.put_status(StatusCode::OK)
                .put_resp_body("{pascal} index")
        }})
    }}

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {{
        Box::pin(async move {{
            conn.put_status(StatusCode::OK)
                .put_resp_body("{pascal} show")
        }})
    }}

    fn create(&self, conn: Conn) -> BoxFuture<'_, Conn> {{
        Box::pin(async move {{
            conn.put_status(StatusCode::CREATED)
                .put_resp_body("{pascal} created")
        }})
    }}
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_controller_file() {
        let output = controller_file("Room");
        assert!(output.contains("RoomController"));
        assert!(output.contains("impl Controller"));
    }
}
