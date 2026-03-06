use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;

use http::StatusCode;
use io_uring::{IoUring, opcode, types};
use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use nix::libc;
use rebar_core::runtime::Runtime;

use crate::endpoint::ErrorHandler;
use crate::http_parse::{self, ParseError};

/// Default peer address used when the actual peer address isn't captured.
/// We use accept without sockaddr storage for maximum performance.
fn default_peer_addr() -> SocketAddr {
    "0.0.0.0:0".parse().unwrap()
}

// ---------------------------------------------------------------------------
// User-data encoding
// ---------------------------------------------------------------------------
// Layout of the 64-bit CQE user_data word:
//   bits  0–7  : ConnState (u8)
//   bits  8–39 : slot index (u32)
//   bits 40–63 : generation (lower 24 bits of u32)

fn encode_user_data(state: u8, slot: u32, generation: u32) -> u64 {
    (state as u64) | ((slot as u64) << 8) | (((generation & 0x00FF_FFFF) as u64) << 40)
}

fn decode_user_data(data: u64) -> (u8, u32, u32) {
    let state = (data & 0xFF) as u8;
    let slot = ((data >> 8) & 0xFFFF_FFFF) as u32;
    let generation = ((data >> 40) & 0x00FF_FFFF) as u32;
    (state, slot, generation)
}

// ---------------------------------------------------------------------------
// ConnState constants
// ---------------------------------------------------------------------------

const STATE_ACCEPTING: u8 = 0;
const STATE_READING: u8 = 1;
const STATE_WRITING: u8 = 2;
const STATE_CLOSING: u8 = 3;

// ---------------------------------------------------------------------------
// ConnSlot
// ---------------------------------------------------------------------------

pub(crate) struct ConnSlot {
    pub fd: RawFd,
    pub generation: u32,
    pub read_buf_idx: u16,
    pub read_len: usize,
    pub write_buf: Vec<u8>,
    pub write_offset: usize,
    pub keep_alive: bool,
}

// ---------------------------------------------------------------------------
// ConnectionPool
// ---------------------------------------------------------------------------

pub(crate) struct ConnectionPool {
    slots: Vec<Option<ConnSlot>>,
    free_list: Vec<u32>,
    next_generation: u32,
}

impl ConnectionPool {
    pub fn new(capacity: usize) -> Self {
        let mut free_list = Vec::with_capacity(capacity);
        // Push in reverse so that index 0 is popped first.
        for i in (0..capacity as u32).rev() {
            free_list.push(i);
        }
        Self {
            slots: (0..capacity).map(|_| None).collect(),
            free_list,
            next_generation: 0,
        }
    }

    pub fn alloc(&mut self) -> Option<u32> {
        self.free_list.pop()
    }

    pub fn free(&mut self, idx: u32) {
        if let Some(slot) = self.slots.get_mut(idx as usize) {
            *slot = None;
        }
        self.free_list.push(idx);
    }

    pub fn get(&self, idx: u32) -> Option<&ConnSlot> {
        self.slots.get(idx as usize).and_then(|s| s.as_ref())
    }

    pub fn get_mut(&mut self, idx: u32) -> Option<&mut ConnSlot> {
        self.slots.get_mut(idx as usize).and_then(|s| s.as_mut())
    }

    /// Allocate a generation counter for a new connection.
    pub fn next_generation(&mut self) -> u32 {
        let g = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        g
    }

    /// Insert a ConnSlot at the given index.
    pub fn insert(&mut self, idx: u32, slot: ConnSlot) {
        if let Some(entry) = self.slots.get_mut(idx as usize) {
            *entry = Some(slot);
        }
    }
}

// ---------------------------------------------------------------------------
// BufferPool
// ---------------------------------------------------------------------------

pub(crate) struct BufferPool {
    data: Vec<u8>,
    chunk_size: usize,
    free_list: Vec<u16>,
}

impl BufferPool {
    pub fn new(count: usize, chunk_size: usize) -> Self {
        let data = vec![0u8; count * chunk_size];
        let mut free_list = Vec::with_capacity(count);
        for i in (0..count as u16).rev() {
            free_list.push(i);
        }
        Self {
            data,
            chunk_size,
            free_list,
        }
    }

    pub fn alloc(&mut self) -> Option<u16> {
        self.free_list.pop()
    }

    pub fn free(&mut self, idx: u16) {
        self.free_list.push(idx);
    }

    pub fn slice(&self, idx: u16) -> &[u8] {
        let start = idx as usize * self.chunk_size;
        &self.data[start..start + self.chunk_size]
    }

    pub fn slice_mut(&mut self, idx: u16) -> &mut [u8] {
        let start = idx as usize * self.chunk_size;
        &mut self.data[start..start + self.chunk_size]
    }

    #[allow(dead_code)]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[allow(dead_code)]
    pub fn total_len(&self) -> usize {
        self.data.len()
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }
}

// ---------------------------------------------------------------------------
// submit helpers
// ---------------------------------------------------------------------------

fn submit_accept(ring: &mut IoUring, listen_fd: RawFd) {
    let entry = opcode::Accept::new(types::Fd(listen_fd), std::ptr::null_mut(), std::ptr::null_mut())
        .build()
        .user_data(encode_user_data(STATE_ACCEPTING, 0, 0));
    unsafe {
        ring.submission().push(&entry).ok();
    }
}

fn submit_read(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *mut u8,
    len: u32,
    offset: usize,
    slot_idx: u32,
    generation: u32,
) {
    let entry = opcode::Read::new(types::Fd(fd), buf_ptr.wrapping_add(offset), len - offset as u32)
        .build()
        .user_data(encode_user_data(STATE_READING, slot_idx, generation));
    unsafe {
        ring.submission().push(&entry).ok();
    }
}

fn submit_write(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *const u8,
    len: u32,
    slot_idx: u32,
    generation: u32,
) {
    let entry = opcode::Write::new(types::Fd(fd), buf_ptr, len)
        .build()
        .user_data(encode_user_data(STATE_WRITING, slot_idx, generation));
    unsafe {
        ring.submission().push(&entry).ok();
    }
}

fn submit_close(ring: &mut IoUring, fd: RawFd, slot_idx: u32, generation: u32) {
    let entry = opcode::Close::new(types::Fd(fd))
        .build()
        .user_data(encode_user_data(STATE_CLOSING, slot_idx, generation));
    unsafe {
        ring.submission().push(&entry).ok();
    }
}

// ---------------------------------------------------------------------------
// execute_request – runs Conn through the router + after-plugs
// ---------------------------------------------------------------------------

async fn execute_request(
    conn: Conn,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    _runtime: &Arc<Runtime>,
) -> Conn {
    let method = conn.method.clone();
    let path = conn.uri.path().to_string();

    let mut conn = match router.resolve(&method, &path) {
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

// ---------------------------------------------------------------------------
// write_static_error – queue a canned error response and close
// ---------------------------------------------------------------------------

fn write_static_error_and_close(
    ring: &mut IoUring,
    fd: RawFd,
    response: &'static [u8],
    slot_idx: u32,
    generation: u32,
    conn_pool: &mut ConnectionPool,
    buf_pool: &mut BufferPool,
) {
    // Free the read buffer if this slot has one.
    if let Some(slot) = conn_pool.get(slot_idx) {
        buf_pool.free(slot.read_buf_idx);
    }

    // We write the static response directly, then the CLOSING CQE will free the slot.
    // Store the static bytes in the slot's write_buf so the pointer stays valid.
    if let Some(slot) = conn_pool.get_mut(slot_idx) {
        slot.write_buf = response.to_vec();
        slot.write_offset = 0;
        slot.keep_alive = false;
        let ptr = slot.write_buf.as_ptr();
        let len = slot.write_buf.len() as u32;
        submit_write(ring, fd, ptr, len, slot_idx, generation);
    } else {
        // Slot is gone — just close.
        submit_close(ring, fd, slot_idx, generation);
    }
}

// ---------------------------------------------------------------------------
// run_event_loop
// ---------------------------------------------------------------------------

pub(crate) fn run_event_loop(
    ring: &mut IoUring,
    listen_fd: RawFd,
    conn_pool: &mut ConnectionPool,
    buf_pool: &mut BufferPool,
    router: &MahaloRouter,
    error_handler: &Option<ErrorHandler>,
    after_plugs: &[Box<dyn Plug>],
    runtime: &Arc<Runtime>,
    body_limit: usize,
    tokio_rt: &tokio::runtime::Runtime,
) {
    let peer_addr = default_peer_addr();
    // Submit the first accept.
    submit_accept(ring, listen_fd);

    loop {
        // Submit pending SQEs and wait for at least one CQE.
        if let Err(e) = ring.submit_and_wait(1) {
            tracing::error!("io_uring submit_and_wait error: {}", e);
            continue;
        }

        // Drain all available CQEs.
        let cqes: Vec<io_uring::cqueue::Entry> = ring.completion().collect();

        for cqe in cqes {
            let (state, slot_idx, generation) = decode_user_data(cqe.user_data());
            let result = cqe.result();

            match state {
                STATE_ACCEPTING => {
                    if result < 0 {
                        tracing::warn!("accept error: {}", std::io::Error::from_raw_os_error(-result));
                        submit_accept(ring, listen_fd);
                        continue;
                    }
                    let new_fd = result;

                    // Allocate a slot and buffer for the new connection.
                    let slot_idx = match conn_pool.alloc() {
                        Some(idx) => idx,
                        None => {
                            // Pool full — reject with 503 and close immediately.
                            // We cannot store state, so just write + close via raw syscall.
                            unsafe {
                                libc::write(
                                    new_fd,
                                    http_parse::RESPONSE_503.as_ptr() as *const libc::c_void,
                                    http_parse::RESPONSE_503.len(),
                                );
                                libc::close(new_fd);
                            }
                            submit_accept(ring, listen_fd);
                            continue;
                        }
                    };

                    let buf_idx = match buf_pool.alloc() {
                        Some(idx) => idx,
                        None => {
                            conn_pool.free(slot_idx);
                            unsafe {
                                libc::write(
                                    new_fd,
                                    http_parse::RESPONSE_503.as_ptr() as *const libc::c_void,
                                    http_parse::RESPONSE_503.len(),
                                );
                                libc::close(new_fd);
                            }
                            submit_accept(ring, listen_fd);
                            continue;
                        }
                    };

                    let generation = conn_pool.next_generation();

                    conn_pool.insert(
                        slot_idx,
                        ConnSlot {
                            fd: new_fd,
                            generation,
                            read_buf_idx: buf_idx,
                            read_len: 0,
                            write_buf: Vec::new(),
                            write_offset: 0,
                            keep_alive: true,
                        },
                    );

                    // Submit read for the new connection.
                    let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                    let chunk_len = buf_pool.chunk_size() as u32;
                    submit_read(ring, new_fd, buf_ptr, chunk_len, 0, slot_idx, generation);

                    // Resubmit accept for the next connection.
                    submit_accept(ring, listen_fd);
                }

                STATE_READING => {
                    // Validate generation.
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
                        // EOF or error — close connection, free resources.
                        if let Some(slot) = conn_pool.get(slot_idx) {
                            let fd = slot.fd;
                            buf_pool.free(slot.read_buf_idx);
                            submit_close(ring, fd, slot_idx, generation);
                        }
                        continue;
                    }

                    let n = result as usize;
                    let slot = match conn_pool.get_mut(slot_idx) {
                        Some(s) => s,
                        None => continue,
                    };
                    slot.read_len += n;
                    let fd = slot.fd;
                    let buf_idx = slot.read_buf_idx;
                    let read_len = slot.read_len;
                    let chunk_size = buf_pool.chunk_size();

                    let buf = &buf_pool.slice(buf_idx)[..read_len];

                    match http_parse::try_parse_request(buf, body_limit, peer_addr) {
                        Ok(Some(parsed)) => {
                            // Attach runtime to conn.
                            let conn = parsed.conn.with_runtime(Arc::clone(runtime));

                            // Execute through router.
                            let conn = tokio_rt.block_on(
                                execute_request(conn, router, error_handler, after_plugs, runtime),
                            );

                            let response_bytes =
                                http_parse::serialize_response(&conn, parsed.keep_alive);

                            let slot = conn_pool.get_mut(slot_idx).unwrap();
                            slot.write_buf = response_bytes;
                            slot.write_offset = 0;
                            slot.keep_alive = parsed.keep_alive;

                            let ptr = slot.write_buf.as_ptr();
                            let len = slot.write_buf.len() as u32;
                            submit_write(ring, fd, ptr, len, slot_idx, generation);
                        }
                        Ok(None) => {
                            // Partial request — need more data.
                            if read_len >= chunk_size {
                                // Buffer full, can't read more — send 413.
                                write_static_error_and_close(
                                    ring,
                                    fd,
                                    http_parse::RESPONSE_413,
                                    slot_idx,
                                    generation,
                                    conn_pool,
                                    buf_pool,
                                );
                            } else {
                                // Submit another read at the current offset.
                                let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                                submit_read(
                                    ring,
                                    fd,
                                    buf_ptr,
                                    chunk_size as u32,
                                    read_len,
                                    slot_idx,
                                    generation,
                                );
                            }
                        }
                        Err(ParseError::BodyTooLarge) => {
                            write_static_error_and_close(
                                ring,
                                fd,
                                http_parse::RESPONSE_413,
                                slot_idx,
                                generation,
                                conn_pool,
                                buf_pool,
                            );
                        }
                        Err(ParseError::InvalidRequest) => {
                            write_static_error_and_close(
                                ring,
                                fd,
                                http_parse::RESPONSE_400,
                                slot_idx,
                                generation,
                                conn_pool,
                                buf_pool,
                            );
                        }
                    }
                }

                STATE_WRITING => {
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
                        // Write error — close.
                        if let Some(slot) = conn_pool.get(slot_idx) {
                            let fd = slot.fd;
                            buf_pool.free(slot.read_buf_idx);
                            submit_close(ring, fd, slot_idx, generation);
                        }
                        continue;
                    }

                    let slot = match conn_pool.get_mut(slot_idx) {
                        Some(s) => s,
                        None => continue,
                    };
                    slot.write_offset += result as usize;
                    let fd = slot.fd;

                    if slot.write_offset < slot.write_buf.len() {
                        // Partial write — submit the remaining bytes.
                        let ptr = unsafe { slot.write_buf.as_ptr().add(slot.write_offset) };
                        let remaining = (slot.write_buf.len() - slot.write_offset) as u32;
                        submit_write(ring, fd, ptr, remaining, slot_idx, generation);
                    } else if slot.keep_alive {
                        // Response fully written, keep-alive — reset for next request.
                        let buf_idx = slot.read_buf_idx;
                        slot.read_len = 0;
                        slot.write_buf.clear();
                        slot.write_offset = 0;

                        let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                        let chunk_len = buf_pool.chunk_size() as u32;
                        submit_read(ring, fd, buf_ptr, chunk_len, 0, slot_idx, generation);
                    } else {
                        // Not keep-alive — close the connection.
                        buf_pool.free(slot.read_buf_idx);
                        submit_close(ring, fd, slot_idx, generation);
                    }
                }

                STATE_CLOSING => {
                    // Connection closed — free the slot.
                    conn_pool.free(slot_idx);
                }

                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- encode / decode round-trip --

    #[test]
    fn encode_decode_round_trip_basic() {
        let (state, slot, generation) = (STATE_READING, 42u32, 7u32);
        let data = encode_user_data(state, slot, generation);
        let (s, sl, g) = decode_user_data(data);
        assert_eq!(s, state);
        assert_eq!(sl, slot);
        assert_eq!(g, generation);
    }

    #[test]
    fn encode_decode_round_trip_max_slot() {
        let (state, slot, generation) = (STATE_WRITING, 0xFFFF_FFFF, 0x00FF_FFFF);
        let data = encode_user_data(state, slot, generation);
        let (s, sl, g) = decode_user_data(data);
        assert_eq!(s, state);
        assert_eq!(sl, slot);
        assert_eq!(g, generation);
    }

    #[test]
    fn encode_decode_round_trip_zero() {
        let data = encode_user_data(0, 0, 0);
        let (s, sl, g) = decode_user_data(data);
        assert_eq!(s, 0);
        assert_eq!(sl, 0);
        assert_eq!(g, 0);
    }

    #[test]
    fn encode_decode_generation_truncated_to_24_bits() {
        // Generations above 24 bits should be masked.
        let data = encode_user_data(STATE_ACCEPTING, 1, 0x01FF_FFFF);
        let (_, _, g) = decode_user_data(data);
        assert_eq!(g, 0x00FF_FFFF); // upper bits truncated
    }

    // -- ConnectionPool --

    #[test]
    fn connection_pool_alloc_free() {
        let mut pool = ConnectionPool::new(3);
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let c = pool.alloc().unwrap();
        assert!(pool.alloc().is_none()); // exhausted

        pool.free(b);
        let d = pool.alloc().unwrap();
        assert_eq!(d, b);
        assert!(pool.alloc().is_none());

        pool.free(a);
        pool.free(c);
        pool.free(d);
    }

    #[test]
    fn connection_pool_insert_and_get() {
        let mut pool = ConnectionPool::new(2);
        let idx = pool.alloc().unwrap();
        let generation = pool.next_generation();

        pool.insert(
            idx,
            ConnSlot {
                fd: 10,
                generation,
                read_buf_idx: 0,
                read_len: 0,
                write_buf: Vec::new(),
                write_offset: 0,
                keep_alive: true,
            },
        );

        assert_eq!(pool.get(idx).unwrap().fd, 10);
        assert_eq!(pool.get(idx).unwrap().generation, generation);

        pool.get_mut(idx).unwrap().read_len = 42;
        assert_eq!(pool.get(idx).unwrap().read_len, 42);
    }

    #[test]
    fn connection_pool_free_clears_slot() {
        let mut pool = ConnectionPool::new(1);
        let idx = pool.alloc().unwrap();
        pool.insert(
            idx,
            ConnSlot {
                fd: 5,
                generation: 0,
                read_buf_idx: 0,
                read_len: 0,
                write_buf: Vec::new(),
                write_offset: 0,
                keep_alive: false,
            },
        );
        assert!(pool.get(idx).is_some());
        pool.free(idx);
        assert!(pool.get(idx).is_none());
    }

    // -- BufferPool --

    #[test]
    fn buffer_pool_alloc_free() {
        let mut pool = BufferPool::new(2, 64);
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        assert!(pool.alloc().is_none());

        pool.free(a);
        let c = pool.alloc().unwrap();
        assert_eq!(c, a);
        assert!(pool.alloc().is_none());

        pool.free(b);
        pool.free(c);
    }

    #[test]
    fn buffer_pool_slice_boundaries() {
        let mut pool = BufferPool::new(4, 128);
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();

        // Write to slice a.
        pool.slice_mut(a)[0] = 0xAA;
        pool.slice_mut(a)[127] = 0xBB;

        // Write to slice b.
        pool.slice_mut(b)[0] = 0xCC;

        // Verify isolation.
        assert_eq!(pool.slice(a)[0], 0xAA);
        assert_eq!(pool.slice(a)[127], 0xBB);
        assert_eq!(pool.slice(b)[0], 0xCC);
        assert_eq!(pool.slice(a).len(), 128);
        assert_eq!(pool.slice(b).len(), 128);
    }

    #[test]
    fn buffer_pool_total_len_and_chunk_size() {
        let pool = BufferPool::new(8, 256);
        assert_eq!(pool.total_len(), 8 * 256);
        assert_eq!(pool.chunk_size(), 256);
        assert!(!pool.as_ptr().is_null());
    }
}
