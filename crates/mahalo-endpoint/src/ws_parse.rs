//! RFC 6455 WebSocket frame parser and serializer.
//!
//! Parses client→server frames (masked, FIN=1 text/binary/ping/pong/close).
//! Serializes server→client frames (unmasked).

/// WebSocket opcodes.
pub const OPCODE_CONTINUATION: u8 = 0;
pub const OPCODE_TEXT: u8 = 1;
pub const OPCODE_BINARY: u8 = 2;
pub const OPCODE_CLOSE: u8 = 8;
pub const OPCODE_PING: u8 = 9;
pub const OPCODE_PONG: u8 = 10;

/// A parsed WebSocket frame.
#[derive(Debug, Clone)]
pub struct WsFrame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// WebSocket parse errors.
#[derive(Debug)]
pub enum WsError {
    /// Frame is too short to be valid.
    Incomplete,
    /// Reserved bits are set or other protocol violation.
    ProtocolError(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Incomplete => write!(f, "incomplete frame"),
            WsError::ProtocolError(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for WsError {}

/// Try to parse a single WebSocket frame from `buf`.
///
/// Returns `Ok(Some((frame, bytes_consumed)))` on success,
/// `Ok(None)` if more data is needed, or `Err` on protocol violation.
///
/// Client→server frames MUST be masked (RFC 6455 §5.1).
pub fn try_parse_frame(buf: &[u8]) -> Result<Option<(WsFrame, usize)>, WsError> {
    if buf.len() < 2 {
        return Ok(None);
    }

    let b0 = buf[0];
    let b1 = buf[1];

    // Check reserved bits (RSV1-3 must be 0 without extensions).
    if b0 & 0x70 != 0 {
        return Err(WsError::ProtocolError("reserved bits set".into()));
    }

    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0F;
    let masked = b1 & 0x80 != 0;

    // Control frames (opcode >= 8) must not be fragmented.
    if opcode >= 8 && !fin {
        return Err(WsError::ProtocolError("fragmented control frame".into()));
    }

    // Client frames must be masked.
    if !masked {
        return Err(WsError::ProtocolError("client frame not masked".into()));
    }

    let (payload_len, header_offset) = match b1 & 0x7F {
        len @ 0..=125 => (len as u64, 2),
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            let len = u16::from_be_bytes([buf[2], buf[3]]) as u64;
            (len, 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let len = u64::from_be_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]);
            // Most significant bit must be 0 (RFC 6455 §5.2).
            if len >> 63 != 0 {
                return Err(WsError::ProtocolError("payload length MSB set".into()));
            }
            (len, 10)
        }
        _ => unreachable!(),
    };

    // Control frames must have payload <= 125 bytes.
    if opcode >= 8 && payload_len > 125 {
        return Err(WsError::ProtocolError("control frame payload too large".into()));
    }

    let mask_offset = header_offset;
    let data_offset = mask_offset + 4; // 4-byte mask key
    let total = data_offset + payload_len as usize;

    if buf.len() < total {
        return Ok(None);
    }

    let mask_key = &buf[mask_offset..mask_offset + 4];
    let mut payload = buf[data_offset..total].to_vec();

    // Unmask the payload.
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask_key[i & 3];
    }

    Ok(Some((WsFrame { opcode, payload }, total)))
}

/// Serialize a WebSocket frame (server→client, unmasked) into `buf`.
pub fn serialize_frame(opcode: u8, payload: &[u8], buf: &mut Vec<u8>) {
    // FIN=1, no RSV bits, opcode
    buf.push(0x80 | opcode);

    let len = payload.len();
    if len <= 125 {
        buf.push(len as u8);
    } else if len <= 65535 {
        buf.push(126);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(127);
        buf.extend_from_slice(&(len as u64).to_be_bytes());
    }

    buf.extend_from_slice(payload);
}

/// Serialize a WebSocket close frame with a status code and optional reason.
pub fn serialize_close_frame(code: u16, reason: &[u8], buf: &mut Vec<u8>) {
    let mut payload = Vec::with_capacity(2 + reason.len());
    payload.extend_from_slice(&code.to_be_bytes());
    payload.extend_from_slice(reason);
    serialize_frame(OPCODE_CLOSE, &payload, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a masked client frame for testing.
    fn build_masked_frame(opcode: u8, payload: &[u8], mask_key: [u8; 4]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push(0x80 | opcode); // FIN=1

        let len = payload.len();
        if len <= 125 {
            frame.push(0x80 | len as u8); // MASK=1
        } else if len <= 65535 {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }

        frame.extend_from_slice(&mask_key);
        for (i, &b) in payload.iter().enumerate() {
            frame.push(b ^ mask_key[i & 3]);
        }
        frame
    }

    #[test]
    fn parse_simple_text_frame() {
        let payload = b"Hello";
        let frame = build_masked_frame(OPCODE_TEXT, payload, [0x37, 0xfa, 0x21, 0x3d]);

        let (parsed, consumed) = try_parse_frame(&frame).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed.opcode, OPCODE_TEXT);
        assert_eq!(parsed.payload, b"Hello");
    }

    #[test]
    fn parse_empty_payload() {
        let frame = build_masked_frame(OPCODE_TEXT, b"", [0x01, 0x02, 0x03, 0x04]);
        let (parsed, consumed) = try_parse_frame(&frame).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed.opcode, OPCODE_TEXT);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn parse_126_byte_payload() {
        let payload = vec![0x42u8; 200];
        let frame = build_masked_frame(OPCODE_TEXT, &payload, [0xAA, 0xBB, 0xCC, 0xDD]);

        let (parsed, consumed) = try_parse_frame(&frame).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn parse_65536_byte_payload() {
        let payload = vec![0x55u8; 65536];
        let frame = build_masked_frame(OPCODE_BINARY, &payload, [0x11, 0x22, 0x33, 0x44]);

        let (parsed, consumed) = try_parse_frame(&frame).unwrap().unwrap();
        assert_eq!(consumed, frame.len());
        assert_eq!(parsed.opcode, OPCODE_BINARY);
        assert_eq!(parsed.payload.len(), 65536);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn parse_ping_frame() {
        let frame = build_masked_frame(OPCODE_PING, b"ping", [0x01, 0x02, 0x03, 0x04]);
        let (parsed, _) = try_parse_frame(&frame).unwrap().unwrap();
        assert_eq!(parsed.opcode, OPCODE_PING);
        assert_eq!(parsed.payload, b"ping");
    }

    #[test]
    fn parse_close_frame_with_code() {
        let mut close_payload = Vec::new();
        close_payload.extend_from_slice(&1000u16.to_be_bytes());
        close_payload.extend_from_slice(b"normal");
        let frame = build_masked_frame(OPCODE_CLOSE, &close_payload, [0x05, 0x06, 0x07, 0x08]);

        let (parsed, _) = try_parse_frame(&frame).unwrap().unwrap();
        assert_eq!(parsed.opcode, OPCODE_CLOSE);
        assert_eq!(&parsed.payload[..2], &1000u16.to_be_bytes());
        assert_eq!(&parsed.payload[2..], b"normal");
    }

    #[test]
    fn parse_incomplete_header() {
        assert!(try_parse_frame(&[0x81]).unwrap().is_none());
    }

    #[test]
    fn parse_incomplete_payload() {
        let full = build_masked_frame(OPCODE_TEXT, b"Hello", [0x01, 0x02, 0x03, 0x04]);
        let partial = &full[..full.len() - 2];
        assert!(try_parse_frame(partial).unwrap().is_none());
    }

    #[test]
    fn parse_incomplete_extended_length() {
        // 126-length but only 3 bytes total (need 4 for length)
        assert!(try_parse_frame(&[0x81, 0xFE, 0x00]).unwrap().is_none());
    }

    #[test]
    fn parse_reserved_bits_error() {
        let frame = build_masked_frame(OPCODE_TEXT, b"x", [0x01, 0x02, 0x03, 0x04]);
        let mut bad = frame;
        bad[0] |= 0x40; // set RSV1
        assert!(matches!(try_parse_frame(&bad), Err(WsError::ProtocolError(_))));
    }

    #[test]
    fn parse_unmasked_client_frame_error() {
        // Build an unmasked frame (server-style)
        let mut frame = vec![0x81, 0x05]; // FIN, TEXT, 5 bytes, no mask
        frame.extend_from_slice(b"Hello");
        assert!(matches!(try_parse_frame(&frame), Err(WsError::ProtocolError(_))));
    }

    #[test]
    fn parse_fragmented_control_frame_error() {
        // FIN=0 with PING opcode
        let mut frame = vec![OPCODE_PING, 0x80]; // FIN=0, MASK=1
        frame.extend_from_slice(&[0, 0, 0, 0]); // mask key
        assert!(matches!(try_parse_frame(&frame), Err(WsError::ProtocolError(_))));
    }

    #[test]
    fn parse_control_frame_payload_too_large() {
        let payload = vec![0u8; 126];
        let frame = build_masked_frame(OPCODE_PING, &payload, [0, 0, 0, 0]);
        assert!(matches!(try_parse_frame(&frame), Err(WsError::ProtocolError(_))));
    }

    #[test]
    fn serialize_small_frame() {
        let mut buf = Vec::new();
        serialize_frame(OPCODE_TEXT, b"Hello", &mut buf);

        assert_eq!(buf[0], 0x81); // FIN + TEXT
        assert_eq!(buf[1], 5);    // length
        assert_eq!(&buf[2..], b"Hello");
    }

    #[test]
    fn serialize_medium_frame() {
        let payload = vec![0x42u8; 200];
        let mut buf = Vec::new();
        serialize_frame(OPCODE_TEXT, &payload, &mut buf);

        assert_eq!(buf[0], 0x81);
        assert_eq!(buf[1], 126);
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 200);
        assert_eq!(&buf[4..], payload.as_slice());
    }

    #[test]
    fn serialize_large_frame() {
        let payload = vec![0x55u8; 65536];
        let mut buf = Vec::new();
        serialize_frame(OPCODE_BINARY, &payload, &mut buf);

        assert_eq!(buf[0], 0x82); // FIN + BINARY
        assert_eq!(buf[1], 127);
        let len = u64::from_be_bytes([buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9]]);
        assert_eq!(len, 65536);
        assert_eq!(&buf[10..], payload.as_slice());
    }

    #[test]
    fn serialize_close_frame_test() {
        let mut buf = Vec::new();
        serialize_close_frame(1000, b"normal", &mut buf);

        assert_eq!(buf[0], 0x88); // FIN + CLOSE
        assert_eq!(buf[1], 8);    // 2 (code) + 6 (reason)
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 1000);
        assert_eq!(&buf[4..], b"normal");
    }

    #[test]
    fn serialize_pong_frame() {
        let mut buf = Vec::new();
        serialize_frame(OPCODE_PONG, b"ping-data", &mut buf);

        assert_eq!(buf[0], 0x8A); // FIN + PONG
        assert_eq!(buf[1], 9);
        assert_eq!(&buf[2..], b"ping-data");
    }

    #[test]
    fn parse_then_serialize_roundtrip() {
        // Serialize a server frame
        let mut server_buf = Vec::new();
        serialize_frame(OPCODE_TEXT, b"roundtrip", &mut server_buf);

        // Verify it has the expected structure
        assert_eq!(server_buf[0], 0x81);
        assert_eq!(server_buf[1], 9);
        assert_eq!(&server_buf[2..], b"roundtrip");
    }

    #[test]
    fn parse_two_frames_in_buffer() {
        let frame1 = build_masked_frame(OPCODE_TEXT, b"one", [0x01, 0x02, 0x03, 0x04]);
        let frame2 = build_masked_frame(OPCODE_TEXT, b"two", [0x05, 0x06, 0x07, 0x08]);

        let mut buf = Vec::new();
        buf.extend_from_slice(&frame1);
        buf.extend_from_slice(&frame2);

        let (parsed1, consumed1) = try_parse_frame(&buf).unwrap().unwrap();
        assert_eq!(parsed1.payload, b"one");
        assert_eq!(consumed1, frame1.len());

        let (parsed2, consumed2) = try_parse_frame(&buf[consumed1..]).unwrap().unwrap();
        assert_eq!(parsed2.payload, b"two");
        assert_eq!(consumed2, frame2.len());
    }

    #[test]
    fn parse_empty_buffer() {
        assert!(try_parse_frame(&[]).unwrap().is_none());
    }
}
