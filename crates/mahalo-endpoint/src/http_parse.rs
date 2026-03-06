use std::net::SocketAddr;
use std::str::FromStr;

use bytes::Bytes;
use http::{HeaderMap, Method, Uri};
use mahalo_core::conn::Conn;

/// Result of successfully parsing a complete HTTP/1.1 request.
pub struct ParsedRequest {
    pub conn: Conn,
    pub keep_alive: bool,
    pub bytes_consumed: usize,
}

/// Errors that can occur during HTTP request parsing.
#[derive(Debug)]
pub enum ParseError {
    InvalidRequest,
    BodyTooLarge,
}

/// Pre-built 400 Bad Request response.
pub const RESPONSE_400: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 11\r\nconnection: close\r\n\r\nBad Request";

/// Pre-built 413 Payload Too Large response.
pub const RESPONSE_413: &[u8] =
    b"HTTP/1.1 413 Payload Too Large\r\ncontent-length: 17\r\nconnection: close\r\n\r\nPayload Too Large";

/// Pre-built 503 Service Unavailable response.
pub const RESPONSE_503: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 19\r\nconnection: close\r\n\r\nService Unavailable";

/// Attempt to parse an HTTP/1.1 request from `buf`.
///
/// Returns `Ok(None)` if the buffer contains a partial request (need more data).
/// Returns `Ok(Some(parsed))` on a complete request.
/// Returns `Err` on invalid input or body exceeding `body_limit`.
pub fn try_parse_request(
    buf: &[u8],
    body_limit: usize,
    peer_addr: SocketAddr,
) -> Result<Option<ParsedRequest>, ParseError> {
    let mut headers_buf = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers_buf);

    let header_len = match req.parse(buf) {
        Ok(httparse::Status::Partial) => return Ok(None),
        Ok(httparse::Status::Complete(len)) => len,
        Err(_) => return Err(ParseError::InvalidRequest),
    };

    let method = Method::from_bytes(req.method.ok_or(ParseError::InvalidRequest)?.as_bytes())
        .map_err(|_| ParseError::InvalidRequest)?;
    let uri = Uri::from_str(req.path.ok_or(ParseError::InvalidRequest)?)
        .map_err(|_| ParseError::InvalidRequest)?;

    let mut header_map = HeaderMap::with_capacity(req.headers.len());
    let mut content_length: Option<usize> = None;
    let mut connection_header: Option<&str> = None;

    for h in req.headers.iter() {
        let name = http::header::HeaderName::from_bytes(h.name.as_bytes())
            .map_err(|_| ParseError::InvalidRequest)?;
        let value = http::header::HeaderValue::from_bytes(h.value)
            .map_err(|_| ParseError::InvalidRequest)?;

        if h.name.eq_ignore_ascii_case("content-length") {
            if let Ok(s) = std::str::from_utf8(h.value) {
                content_length = s.trim().parse().ok();
            }
        }
        if h.name.eq_ignore_ascii_case("connection") {
            connection_header = std::str::from_utf8(h.value).ok();
        }

        header_map.append(name, value);
    }

    // Determine if the method carries a body.
    let body_len = match method {
        Method::GET | Method::HEAD | Method::DELETE => 0,
        _ => content_length.unwrap_or(0),
    };

    if body_len > body_limit {
        return Err(ParseError::BodyTooLarge);
    }

    let total = header_len + body_len;

    // Not enough data yet for the full body.
    if buf.len() < total {
        return Ok(None);
    }

    let body = if body_len > 0 {
        Bytes::copy_from_slice(&buf[header_len..total])
    } else {
        Bytes::new()
    };

    // HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close.
    let keep_alive = match connection_header {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        _ => {
            // Check HTTP version from httparse (1 = HTTP/1.1, 0 = HTTP/1.0).
            req.version.unwrap_or(1) >= 1
        }
    };

    let mut conn = Conn::new(method, uri);
    conn.headers = header_map;
    conn.body = body;
    conn.remote_addr = Some(peer_addr);

    Ok(Some(ParsedRequest {
        conn,
        keep_alive,
        bytes_consumed: total,
    }))
}

/// Fast status line lookup for common HTTP status codes (avoids allocation).
#[inline]
fn status_line(code: u16) -> &'static [u8] {
    match code {
        200 => b"HTTP/1.1 200 OK\r\n",
        201 => b"HTTP/1.1 201 Created\r\n",
        204 => b"HTTP/1.1 204 No Content\r\n",
        301 => b"HTTP/1.1 301 Moved Permanently\r\n",
        302 => b"HTTP/1.1 302 Found\r\n",
        304 => b"HTTP/1.1 304 Not Modified\r\n",
        400 => b"HTTP/1.1 400 Bad Request\r\n",
        401 => b"HTTP/1.1 401 Unauthorized\r\n",
        403 => b"HTTP/1.1 403 Forbidden\r\n",
        404 => b"HTTP/1.1 404 Not Found\r\n",
        405 => b"HTTP/1.1 405 Method Not Allowed\r\n",
        413 => b"HTTP/1.1 413 Payload Too Large\r\n",
        500 => b"HTTP/1.1 500 Internal Server Error\r\n",
        503 => b"HTTP/1.1 503 Service Unavailable\r\n",
        _ => b"",
    }
}

/// Write a usize as decimal digits directly into buf (no String allocation).
#[inline]
fn write_usize(buf: &mut Vec<u8>, mut n: usize) {
    if n == 0 {
        buf.push(b'0');
        return;
    }
    // Max 20 digits for u64.
    let start = buf.len();
    while n > 0 {
        buf.push(b'0' + (n % 10) as u8);
        n /= 10;
    }
    buf[start..].reverse();
}

/// Write a u16 as 3-digit decimal directly into buf (for HTTP status codes).
#[inline]
fn write_status_code(buf: &mut Vec<u8>, code: u16) {
    buf.push(b'0' + (code / 100) as u8);
    buf.push(b'0' + ((code / 10) % 10) as u8);
    buf.push(b'0' + (code % 10) as u8);
}

/// Serialize a Conn's response into a raw HTTP/1.1 response buffer.
///
/// Zero-allocation for common status codes (200, 404, etc.) — uses
/// pre-computed status lines and manual integer formatting.
pub fn serialize_response(conn: &Conn, keep_alive: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    serialize_response_into(conn, keep_alive, &mut buf);
    buf
}

/// Serialize a Conn's response into an existing buffer (avoids allocation on reuse).
///
/// Clears `buf` first, then writes the full HTTP/1.1 response. The existing
/// capacity is preserved, so repeated calls on the same Vec avoid re-allocation.
pub fn serialize_response_into(conn: &Conn, keep_alive: bool, buf: &mut Vec<u8>) {
    let code = conn.status.as_u16();
    let body = &conn.resp_body;
    let body_len = body.len();
    let has_content_length = conn.resp_headers.contains_key(http::header::CONTENT_LENGTH);
    let connection_hdr = if keep_alive {
        &b"connection: keep-alive\r\n"[..]
    } else {
        &b"connection: close\r\n"[..]
    };

    // Estimate needed capacity.
    let mut header_bytes = 0;
    for (name, value) in conn.resp_headers.iter() {
        header_bytes += name.as_str().len() + 2 + value.len() + 2;
    }
    let needed = 128 + header_bytes + body_len;

    buf.clear();
    buf.reserve(needed.saturating_sub(buf.capacity()));

    // Status line — try pre-computed, fall back to manual.
    let precomputed = status_line(code);
    if !precomputed.is_empty() {
        buf.extend_from_slice(precomputed);
    } else {
        buf.extend_from_slice(b"HTTP/1.1 ");
        write_status_code(buf, code);
        buf.push(b' ');
        let reason = conn.status.canonical_reason().unwrap_or("Unknown");
        buf.extend_from_slice(reason.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Response headers.
    for (name, value) in conn.resp_headers.iter() {
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    // Content-length (manual integer formatting, no String alloc).
    if !has_content_length {
        buf.extend_from_slice(b"content-length: ");
        write_usize(buf, body_len);
        buf.extend_from_slice(b"\r\n");
    }

    // Connection header.
    buf.extend_from_slice(connection_hdr);

    // Separator + body.
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(body);
}


#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    fn addr() -> SocketAddr {
        "127.0.0.1:8080".parse().unwrap()
    }

    #[test]
    fn parse_complete_get_request() {
        let raw = b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();

        assert_eq!(result.conn.method, Method::GET);
        assert_eq!(result.conn.uri, "/hello");
        assert!(result.conn.body.is_empty());
        assert!(result.keep_alive);
        assert_eq!(result.bytes_consumed, raw.len());
        assert_eq!(result.conn.remote_addr, Some(addr()));
    }

    #[test]
    fn parse_post_request_with_body() {
        let body = b"hello world";
        let header = b"POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: 11\r\n\r\n";
        let mut raw = Vec::new();
        raw.extend_from_slice(header);
        raw.extend_from_slice(body);

        let result = try_parse_request(&raw, 1024, addr()).unwrap().unwrap();

        assert_eq!(result.conn.method, Method::POST);
        assert_eq!(result.conn.uri, "/api");
        assert_eq!(result.conn.body.as_ref(), b"hello world");
        assert_eq!(result.bytes_consumed, raw.len());
    }

    #[test]
    fn parse_partial_request_returns_none() {
        let raw = b"GET /hello HT";
        let result = try_parse_request(raw, 1024, addr()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_partial_body_returns_none() {
        let raw = b"POST /api HTTP/1.1\r\nContent-Length: 100\r\n\r\nshort";
        let result = try_parse_request(raw, 1024, addr()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_body_too_large_returns_error() {
        let raw = b"POST /api HTTP/1.1\r\nContent-Length: 2000\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr());
        assert!(matches!(result, Err(ParseError::BodyTooLarge)));
    }

    #[test]
    fn keep_alive_http11_default() {
        let raw = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert!(result.keep_alive);
    }

    #[test]
    fn keep_alive_connection_close() {
        let raw = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert!(!result.keep_alive);
    }

    #[test]
    fn keep_alive_connection_keep_alive_explicit() {
        let raw = b"GET / HTTP/1.0\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert!(result.keep_alive);
    }

    #[test]
    fn serialize_response_round_trip() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::OK)
            .put_resp_header("x-custom", "value")
            .put_resp_body("Hello");

        let buf = serialize_response(&conn, true);
        let response = String::from_utf8(buf).unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("x-custom: value\r\n"));
        assert!(response.contains("content-length: 5\r\n"));
        assert!(response.contains("connection: keep-alive\r\n"));
        assert!(response.ends_with("\r\n\r\nHello"));
    }

    #[test]
    fn serialize_response_connection_close() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::NOT_FOUND)
            .put_resp_body("nope");

        let buf = serialize_response(&conn, false);
        let response = String::from_utf8(buf).unwrap();

        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(response.contains("connection: close\r\n"));
        assert!(response.contains("content-length: 4\r\n"));
    }

    #[test]
    fn serialize_response_respects_existing_content_length() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_resp_header("content-length", "999")
            .put_resp_body("hi");

        let buf = serialize_response(&conn, true);
        let response = String::from_utf8(buf).unwrap();

        // Should use the existing content-length, not add a second one.
        let count = response.matches("content-length").count();
        assert_eq!(count, 1);
        assert!(response.contains("content-length: 999\r\n"));
    }

    #[test]
    fn bytes_consumed_correct_for_get() {
        let raw = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\nextra data";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        // bytes_consumed should NOT include the "extra data" portion.
        let expected = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".len();
        assert_eq!(result.bytes_consumed, expected);
    }

    #[test]
    fn bytes_consumed_correct_for_post() {
        let header = b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n";
        let body = b"abcde";
        let extra = b"leftover";
        let mut raw = Vec::new();
        raw.extend_from_slice(header);
        raw.extend_from_slice(body);
        raw.extend_from_slice(extra);

        let result = try_parse_request(&raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.bytes_consumed, header.len() + body.len());
        assert_eq!(result.conn.body.as_ref(), b"abcde");
    }

    #[test]
    fn static_responses_well_formed() {
        let r400 = std::str::from_utf8(RESPONSE_400).unwrap();
        assert!(r400.starts_with("HTTP/1.1 400"));
        assert!(r400.ends_with("Bad Request"));

        let r413 = std::str::from_utf8(RESPONSE_413).unwrap();
        assert!(r413.starts_with("HTTP/1.1 413"));
        assert!(r413.ends_with("Payload Too Large"));

        let r503 = std::str::from_utf8(RESPONSE_503).unwrap();
        assert!(r503.starts_with("HTTP/1.1 503"));
        assert!(r503.ends_with("Service Unavailable"));
    }
}
