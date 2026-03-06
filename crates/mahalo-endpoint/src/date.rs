use std::cell::RefCell;
use std::time::{Duration, Instant, SystemTime};

const DATE_HEADER_LEN: usize = 29;
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

struct CachedDate {
    bytes: [u8; DATE_HEADER_LEN],
    last_updated: Instant,
}

impl CachedDate {
    fn new() -> Self {
        let mut cd = CachedDate {
            bytes: [0u8; DATE_HEADER_LEN],
            last_updated: Instant::now(),
        };
        cd.refresh();
        cd
    }

    fn refresh(&mut self) {
        let now = SystemTime::now();
        let formatted = httpdate::HttpDate::from(now).to_string();
        let bytes = formatted.as_bytes();
        // httpdate always produces exactly 29 bytes (RFC 7231 IMF-fixdate).
        self.bytes[..DATE_HEADER_LEN].copy_from_slice(&bytes[..DATE_HEADER_LEN]);
        self.last_updated = Instant::now();
    }
}

thread_local! {
    static CACHED: RefCell<CachedDate> = RefCell::new(CachedDate::new());
}

/// Append `date: <IMF-fixdate>\r\n` to `buf`, using a thread-local cache
/// that refreshes every 500ms.
#[inline]
pub fn write_date_header(buf: &mut Vec<u8>) {
    CACHED.with(|cell| {
        let mut cached = cell.borrow_mut();
        if cached.last_updated.elapsed() >= REFRESH_INTERVAL {
            cached.refresh();
        }
        buf.extend_from_slice(b"date: ");
        buf.extend_from_slice(&cached.bytes);
        buf.extend_from_slice(b"\r\n");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_header_format() {
        let mut buf = Vec::new();
        write_date_header(&mut buf);
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.starts_with("date: "));
        assert!(s.ends_with("\r\n"));
        // Total: "date: " (6) + 29 + "\r\n" (2) = 37
        assert_eq!(buf.len(), 37);
    }

    #[test]
    fn date_header_cached_is_stable() {
        let mut buf1 = Vec::new();
        let mut buf2 = Vec::new();
        write_date_header(&mut buf1);
        write_date_header(&mut buf2);
        // Within 500ms, should be identical.
        assert_eq!(buf1, buf2);
    }
}
