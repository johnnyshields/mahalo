use std::net::SocketAddr;
use std::str::FromStr;

use bytes::Bytes;
use http::{HeaderMap, Method, Uri};
use mahalo_core::conn::Conn;

use crate::date::write_date_header;

/// Static server identification header.
const SERVER_HEADER: &[u8] = b"server: mahalo\r\n";

/// Result of successfully parsing a complete HTTP/1.1 request.
pub struct ParsedRequest {
    pub conn: Conn,
    pub keep_alive: bool,
    pub bytes_consumed: usize,
    pub ws_key: Option<String>,
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

/// Internal result of the shared parsing helper — everything extracted from
/// the raw buffer before Conn population.
struct RawParsed<'a> {
    method: Method,
    uri: Uri,
    headers: &'a [httparse::Header<'a>],
    content_length: Option<usize>,
    connection_header: Option<&'a str>,
    http_version: Option<u8>,
    header_len: usize,
}

/// Shared parsing core: runs httparse, extracts method/uri, scans for
/// content-length and connection headers. Does NOT touch Conn or HeaderMap.
#[inline]
fn parse_raw<'buf, 'hdr>(
    req: &'hdr mut httparse::Request<'hdr, 'buf>,
    buf: &'buf [u8],
) -> Result<Option<RawParsed<'hdr>>, ParseError> {
    let header_len = match req.parse(buf) {
        Ok(httparse::Status::Partial) => return Ok(None),
        Ok(httparse::Status::Complete(len)) => len,
        Err(_) => return Err(ParseError::InvalidRequest),
    };

    let method = Method::from_bytes(req.method.ok_or(ParseError::InvalidRequest)?.as_bytes())
        .map_err(|_| ParseError::InvalidRequest)?;
    let uri = Uri::from_str(req.path.ok_or(ParseError::InvalidRequest)?)
        .map_err(|_| ParseError::InvalidRequest)?;

    let mut content_length: Option<usize> = None;
    let mut connection_header: Option<&str> = None;

    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("content-length") {
            if let Ok(s) = std::str::from_utf8(h.value) {
                content_length = s.trim().parse().ok();
            }
        }
        if h.name.eq_ignore_ascii_case("connection") {
            connection_header = std::str::from_utf8(h.value).ok();
        }
    }

    Ok(Some(RawParsed {
        method,
        uri,
        headers: req.headers,
        content_length,
        connection_header,
        http_version: req.version,
        header_len,
    }))
}

/// Fast lookup for common header names — avoids the hash+validation in
/// `HeaderName::from_bytes()` for headers we see on virtually every request.
#[inline]
fn fast_header_name(name: &[u8]) -> Option<http::header::HeaderName> {
    // Match on length first (branch predictor friendly), then content.
    match name.len() {
        4 => {
            if name.eq_ignore_ascii_case(b"host") {
                return Some(http::header::HOST);
            }
        }
        6 => {
            if name.eq_ignore_ascii_case(b"accept") {
                return Some(http::header::ACCEPT);
            }
            if name.eq_ignore_ascii_case(b"cookie") {
                return Some(http::header::COOKIE);
            }
        }
        10 => {
            if name.eq_ignore_ascii_case(b"connection") {
                return Some(http::header::CONNECTION);
            }
            if name.eq_ignore_ascii_case(b"user-agent") {
                return Some(http::header::USER_AGENT);
            }
        }
        12 => {
            if name.eq_ignore_ascii_case(b"content-type") {
                return Some(http::header::CONTENT_TYPE);
            }
        }
        14 => {
            if name.eq_ignore_ascii_case(b"content-length") {
                return Some(http::header::CONTENT_LENGTH);
            }
        }
        15 => {
            if name.eq_ignore_ascii_case(b"accept-encoding") {
                return Some(http::header::ACCEPT_ENCODING);
            }
            if name.eq_ignore_ascii_case(b"accept-language") {
                return Some(http::header::ACCEPT_LANGUAGE);
            }
        }
        _ => {}
    }
    None
}

/// Convert parsed httparse headers into an `http::HeaderMap`.
#[inline]
fn build_header_map(headers: &[httparse::Header<'_>]) -> Result<HeaderMap, ParseError> {
    let mut map = HeaderMap::with_capacity(headers.len());
    for h in headers {
        let name = fast_header_name(h.name.as_bytes())
            .or_else(|| http::header::HeaderName::from_bytes(h.name.as_bytes()).ok())
            .ok_or(ParseError::InvalidRequest)?;
        // SAFETY: httparse already validated that header values contain only
        // visible ASCII characters and spaces/tabs (RFC 7230 field-value).
        // Skipping re-validation avoids redundant work on the hot path.
        let value = unsafe {
            http::header::HeaderValue::from_maybe_shared_unchecked(
                Bytes::copy_from_slice(h.value),
            )
        };
        map.append(name, value);
    }
    Ok(map)
}

/// Append parsed httparse headers into an existing `HeaderMap`.
#[inline]
fn append_headers(map: &mut HeaderMap, headers: &[httparse::Header<'_>]) -> Result<(), ParseError> {
    for h in headers {
        let name = fast_header_name(h.name.as_bytes())
            .or_else(|| http::header::HeaderName::from_bytes(h.name.as_bytes()).ok())
            .ok_or(ParseError::InvalidRequest)?;
        // SAFETY: httparse already validated header values.
        let value = unsafe {
            http::header::HeaderValue::from_maybe_shared_unchecked(
                Bytes::copy_from_slice(h.value),
            )
        };
        map.append(name, value);
    }
    Ok(())
}

/// Compute body length, validate against limit, extract body bytes, and
/// determine keep-alive. Returns `Ok(None)` if the buffer is incomplete.
#[inline]
fn finalize_body_and_keepalive(
    raw: &RawParsed<'_>,
    buf: &[u8],
    body_limit: usize,
) -> Result<Option<(Bytes, bool, usize)>, ParseError> {
    let body_len = match raw.method {
        Method::GET | Method::HEAD | Method::DELETE => 0,
        _ => raw.content_length.unwrap_or(0),
    };

    if body_len > body_limit {
        return Err(ParseError::BodyTooLarge);
    }

    let total = raw.header_len + body_len;

    if buf.len() < total {
        return Ok(None);
    }

    let body = if body_len > 0 {
        Bytes::copy_from_slice(&buf[raw.header_len..total])
    } else {
        Bytes::new()
    };

    // HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close.
    let keep_alive = match raw.connection_header {
        Some(v) if v.eq_ignore_ascii_case("close") => false,
        Some(v) if v.eq_ignore_ascii_case("keep-alive") => true,
        // Check HTTP version from httparse (1 = HTTP/1.1, 0 = HTTP/1.0).
        _ => raw.http_version.unwrap_or(1) >= 1,
    };

    Ok(Some((body, keep_alive, total)))
}

/// Detect WebSocket upgrade from raw parsed headers.
///
/// Returns `Some(key)` when method is GET and headers contain:
/// - `Upgrade: websocket`
/// - `Sec-WebSocket-Key: <key>`
/// - `Sec-WebSocket-Version: 13`
fn detect_ws_key(method: &Method, headers: &[httparse::Header<'_>], header_map: &HeaderMap) -> Option<String> {
    if *method != Method::GET {
        return None;
    }
    let mut upgrade_websocket = false;
    let mut key: Option<String> = None;
    for h in headers {
        if h.name.eq_ignore_ascii_case("upgrade") {
            if let Ok(v) = std::str::from_utf8(h.value) {
                if v.eq_ignore_ascii_case("websocket") {
                    upgrade_websocket = true;
                }
            }
        } else if h.name.eq_ignore_ascii_case("sec-websocket-key") {
            if let Ok(v) = std::str::from_utf8(h.value) {
                key = Some(v.trim().to_string());
            }
        }
    }
    if upgrade_websocket && key.is_some() {
        // RFC 6455 §4.2.1: Sec-WebSocket-Version must be 13.
        let version_ok = header_map.get("sec-websocket-version")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim() == "13");
        if version_ok { key } else { None }
    } else {
        None
    }
}

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
    let mut headers_buf = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut headers_buf);

    let raw = match parse_raw(&mut req, buf)? {
        Some(r) => r,
        None => return Ok(None),
    };

    let (body, keep_alive, total) = match finalize_body_and_keepalive(&raw, buf, body_limit)? {
        Some(t) => t,
        None => return Ok(None),
    };

    let header_map = build_header_map(raw.headers)?;

    let mut conn = Conn::new(raw.method, raw.uri);
    conn.headers = header_map;
    conn.body = body;
    conn.remote_addr = Some(peer_addr);

    let ws_key = detect_ws_key(&conn.method, raw.headers, &conn.headers);

    Ok(Some(ParsedRequest {
        conn,
        keep_alive,
        bytes_consumed: total,
        ws_key,
    }))
}

/// Result of parsing into an existing Conn (avoids allocation).
pub struct ParsedIntoResult {
    pub keep_alive: bool,
    pub bytes_consumed: usize,
    pub ws_key: Option<String>,
}

/// Parse an HTTP request into an existing Conn, reusing its backing allocations.
/// The Conn is `reset()` before populating.
pub fn try_parse_into_conn(
    conn: &mut Conn,
    buf: &[u8],
    body_limit: usize,
    peer_addr: SocketAddr,
) -> Result<Option<ParsedIntoResult>, ParseError> {
    let mut headers_buf = [httparse::EMPTY_HEADER; 96];
    let mut req = httparse::Request::new(&mut headers_buf);

    let raw = match parse_raw(&mut req, buf)? {
        Some(r) => r,
        None => return Ok(None),
    };

    let (body, keep_alive, total) = match finalize_body_and_keepalive(&raw, buf, body_limit)? {
        Some(t) => t,
        None => return Ok(None),
    };

    conn.reset(raw.method, raw.uri);
    append_headers(&mut conn.headers, raw.headers)?;
    conn.body = body;
    conn.remote_addr = Some(peer_addr);

    let ws_key = detect_ws_key(&conn.method, raw.headers, &conn.headers);

    Ok(Some(ParsedIntoResult {
        keep_alive,
        bytes_consumed: total,
        ws_key,
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
#[inline]
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
    if buf.capacity() < needed {
        buf.reserve(needed);
    }

    // Use unsafe pointer-based writing to eliminate per-extend bounds checks.
    // SAFETY: We reserved `needed` bytes above, and carefully track `pos` to
    // never exceed the reserved capacity.
    unsafe {
        let base = buf.as_mut_ptr();
        let mut pos = 0usize;

        // Status line — try pre-computed, fall back to manual.
        let precomputed = status_line(code);
        if !precomputed.is_empty() {
            std::ptr::copy_nonoverlapping(precomputed.as_ptr(), base.add(pos), precomputed.len());
            pos += precomputed.len();
        } else {
            let sl = b"HTTP/1.1 ";
            std::ptr::copy_nonoverlapping(sl.as_ptr(), base.add(pos), sl.len());
            pos += sl.len();
            *base.add(pos) = b'0' + (code / 100) as u8;
            *base.add(pos + 1) = b'0' + ((code / 10) % 10) as u8;
            *base.add(pos + 2) = b'0' + (code % 10) as u8;
            pos += 3;
            *base.add(pos) = b' ';
            pos += 1;
            let reason = conn.status.canonical_reason().unwrap_or("Unknown");
            let rb = reason.as_bytes();
            std::ptr::copy_nonoverlapping(rb.as_ptr(), base.add(pos), rb.len());
            pos += rb.len();
            *base.add(pos) = b'\r';
            *base.add(pos + 1) = b'\n';
            pos += 2;
        }

        // Response headers.
        for (name, value) in conn.resp_headers.iter() {
            let nb = name.as_str().as_bytes();
            std::ptr::copy_nonoverlapping(nb.as_ptr(), base.add(pos), nb.len());
            pos += nb.len();
            *base.add(pos) = b':';
            *base.add(pos + 1) = b' ';
            pos += 2;
            let vb = value.as_bytes();
            std::ptr::copy_nonoverlapping(vb.as_ptr(), base.add(pos), vb.len());
            pos += vb.len();
            *base.add(pos) = b'\r';
            *base.add(pos + 1) = b'\n';
            pos += 2;
        }

        // Content-length.
        if !has_content_length {
            let cl = b"content-length: ";
            std::ptr::copy_nonoverlapping(cl.as_ptr(), base.add(pos), cl.len());
            pos += cl.len();
            let mut itoa_buf = itoa::Buffer::new();
            let num = itoa_buf.format(body_len).as_bytes();
            std::ptr::copy_nonoverlapping(num.as_ptr(), base.add(pos), num.len());
            pos += num.len();
            *base.add(pos) = b'\r';
            *base.add(pos + 1) = b'\n';
            pos += 2;
        }

        // Connection header.
        std::ptr::copy_nonoverlapping(connection_hdr.as_ptr(), base.add(pos), connection_hdr.len());
        pos += connection_hdr.len();

        // Update length so write_date_header can extend safely.
        buf.set_len(pos);
    }

    // Date header (thread-local cache, refreshed every 500ms).
    write_date_header(buf);

    // Server header (static bytes, zero cost).
    buf.extend_from_slice(SERVER_HEADER);

    // Separator + body.
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(body);
}


/// Serialize SSE response headers into an existing buffer.
///
/// Writes status line + response headers + `connection: close` + date + server.
/// No `Content-Length` (streaming). No `Transfer-Encoding: chunked` (raw SSE).
pub fn serialize_sse_headers_into(conn: &Conn, buf: &mut Vec<u8>) {
    let code = conn.status.as_u16();

    let mut header_bytes = 0;
    for (name, value) in conn.resp_headers.iter() {
        header_bytes += name.as_str().len() + 2 + value.len() + 2;
    }
    let needed = 128 + header_bytes;

    buf.clear();
    if buf.capacity() < needed {
        buf.reserve(needed);
    }

    let precomputed = status_line(code);
    if !precomputed.is_empty() {
        buf.extend_from_slice(precomputed);
    } else {
        buf.extend_from_slice(b"HTTP/1.1 ");
        buf.push(b'0' + (code / 100) as u8);
        buf.push(b'0' + ((code / 10) % 10) as u8);
        buf.push(b'0' + (code % 10) as u8);
        buf.push(b' ');
        let reason = conn.status.canonical_reason().unwrap_or("Unknown");
        buf.extend_from_slice(reason.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    for (name, value) in conn.resp_headers.iter() {
        buf.extend_from_slice(name.as_str().as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    buf.extend_from_slice(b"connection: close\r\n");
    write_date_header(buf);
    buf.extend_from_slice(SERVER_HEADER);
    buf.extend_from_slice(b"\r\n");
}

/// Serialize a WebSocket upgrade response (HTTP 101 Switching Protocols).
///
/// Computes the `Sec-WebSocket-Accept` value per RFC 6455 §4.2.2:
/// Base64(SHA-1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))
pub fn serialize_ws_accept_response(ws_key: &str, buf: &mut Vec<u8>) {
    use sha1::{Sha1, Digest};
    use base64::Engine;

    let mut hasher = Sha1::new();
    hasher.update(ws_key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let hash = hasher.finalize();
    let accept = base64::engine::general_purpose::STANDARD.encode(hash);

    buf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
    buf.extend_from_slice(b"Upgrade: websocket\r\n");
    buf.extend_from_slice(b"Connection: Upgrade\r\n");
    buf.extend_from_slice(b"Sec-WebSocket-Accept: ");
    buf.extend_from_slice(accept.as_bytes());
    buf.extend_from_slice(b"\r\n\r\n");
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
    fn serialize_response_into_reuses_buffer_capacity() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::OK)
            .put_resp_body("Hello");

        let mut buf = Vec::with_capacity(512);
        serialize_response_into(&conn, true, &mut buf);
        let first_len = buf.len();
        let cap_after_first = buf.capacity();

        // Second call reuses the same buffer — capacity should not grow
        // for a same-size response.
        serialize_response_into(&conn, true, &mut buf);
        assert_eq!(buf.len(), first_len);
        assert_eq!(buf.capacity(), cap_after_first);
    }

    #[test]
    fn serialize_response_includes_date_and_server() {
        let conn = Conn::new(Method::GET, Uri::from_static("/"))
            .put_status(StatusCode::OK)
            .put_resp_body("ok");

        let buf = serialize_response(&conn, true);
        let response = String::from_utf8(buf).unwrap();

        assert!(response.contains("date: "), "missing date header");
        assert!(response.contains("server: mahalo\r\n"), "missing server header");
    }

    #[test]
    fn try_parse_into_conn_basic() {
        let raw = b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut conn = Conn::new(Method::POST, Uri::from_static("/old"));
        conn.resp_headers.insert("x-old", "val".parse().unwrap());

        let result = try_parse_into_conn(&mut conn, raw, 1024, addr())
            .unwrap()
            .unwrap();

        assert_eq!(conn.method, Method::GET);
        assert_eq!(conn.uri, "/hello");
        assert!(conn.body.is_empty());
        assert!(result.keep_alive);
        assert_eq!(result.bytes_consumed, raw.len());
        // Old response headers should be cleared.
        assert!(conn.resp_headers.is_empty());
    }

    #[test]
    fn try_parse_into_conn_partial_returns_none() {
        let raw = b"GET /hello HT";
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = try_parse_into_conn(&mut conn, raw, 1024, addr()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn try_parse_into_conn_body_too_large() {
        let raw = b"POST /api HTTP/1.1\r\nContent-Length: 2000\r\n\r\n";
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = try_parse_into_conn(&mut conn, raw, 1024, addr());
        assert!(matches!(result, Err(ParseError::BodyTooLarge)));
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

    #[test]
    fn parse_websocket_upgrade_request() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, Some("dGhlIHNhbXBsZSBub25jZQ==".to_string()));
    }

    #[test]
    fn parse_normal_request_no_ws_key() {
        let raw = b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None);
    }

    #[test]
    fn ws_accept_response_rfc6455_vector() {
        // RFC 6455 §4.2.2 example: key "dGhlIHNhbXBsZSBub25jZQ==" → accept "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        let mut buf = Vec::new();
        serialize_ws_accept_response("dGhlIHNhbXBsZSBub25jZQ==", &mut buf);
        let response = String::from_utf8(buf).unwrap();
        assert!(response.contains("101 Switching Protocols"));
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        assert!(response.contains("Upgrade: websocket"));
        assert!(response.contains("Connection: Upgrade"));
    }

    #[test]
    fn ws_upgrade_rejected_for_post_method() {
        let raw = b"POST /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None, "POST with Upgrade should not produce ws_key");
    }

    #[test]
    fn ws_upgrade_rejected_without_version_13() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None, "Version != 13 should not produce ws_key");
    }

    #[test]
    fn ws_upgrade_rejected_without_version_header() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let result = try_parse_request(raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None, "Missing version header should not produce ws_key");
    }

    // -- try_parse_into_conn WebSocket detection tests --

    #[test]
    fn parse_into_conn_detects_ws_upgrade() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = try_parse_into_conn(&mut conn, raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, Some("dGhlIHNhbXBsZSBub25jZQ==".to_string()));
    }

    #[test]
    fn parse_into_conn_normal_get_no_ws_key() {
        let raw = b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = try_parse_into_conn(&mut conn, raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None);
    }

    #[test]
    fn parse_into_conn_post_with_websocket_body_no_ws_key() {
        // POST body containing "websocket" must NOT trigger WS detection.
        let body = b"{\"type\":\"websocket\"}";
        let header = format!(
            "POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut raw = Vec::new();
        raw.extend_from_slice(header.as_bytes());
        raw.extend_from_slice(body);

        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = try_parse_into_conn(&mut conn, &raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None, "POST with websocket in body should not produce ws_key");
    }

    #[test]
    fn parse_into_conn_ws_rejected_without_version_13() {
        let raw = b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 8\r\n\r\n";
        let mut conn = Conn::new(Method::GET, Uri::from_static("/"));
        let result = try_parse_into_conn(&mut conn, raw, 1024, addr()).unwrap().unwrap();
        assert_eq!(result.ws_key, None, "Version != 13 should not produce ws_key");
    }
}
