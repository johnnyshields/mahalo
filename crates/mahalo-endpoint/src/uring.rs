use std::collections::VecDeque;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::sync::Arc;

use io_uring::{IoUring, opcode, types};
use mahalo_core::conn::Conn;
use mahalo_core::plug::Plug;
use mahalo_router::MahaloRouter;
use nix::libc;
use rebar_core::gen_server;
use rebar_core::process::ProcessId;
use rebar_core::runtime::Runtime;
use tokio::sync::mpsc;

use mahalo_channel::socket::WsSendItem;

use crate::endpoint::{ErrorHandler, WsConfig};
use crate::http_parse::{self, ParseError};
use crate::ws_parse;

/// Max number of recycled Conn objects per event loop thread.
const CONN_POOL_CAP: usize = 128;
/// Max number of recycled write buffers per event loop thread.
const WRITE_BUF_POOL_CAP: usize = 128;

/// Default peer address used when the actual peer address isn't captured.
fn default_peer_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 0))
}

/// Set TCP_NODELAY on a raw fd (best-effort).
#[inline]
fn set_tcp_nodelay(fd: RawFd) {
    unsafe {
        let flag: libc::c_int = 1;
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &flag as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

// ---------------------------------------------------------------------------
// User-data encoding
// ---------------------------------------------------------------------------
// Layout of the 64-bit CQE user_data word:
//   bits  0–7  : ConnState (u8)
//   bits  8–39 : slot index (u32)
//   bits 40–63 : generation (lower 24 bits of u32)

#[inline]
fn encode_user_data(state: u8, slot: u32, generation: u32) -> u64 {
    (state as u64) | ((slot as u64) << 8) | (((generation & 0x00FF_FFFF) as u64) << 40)
}

#[inline]
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
const STATE_WS_UPGRADING: u8 = 4;
const STATE_WS_READING: u8 = 5;
const STATE_WS_WRITING: u8 = 6;

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
    /// WebSocket state, set after HTTP upgrade completes.
    pub ws: Option<Box<WsState>>,
}

/// Per-connection WebSocket state.
pub(crate) struct WsState {
    /// GenServer process for this connection.
    gen_pid: ProcessId,
    /// Receiver for outbound JSON strings (from ChannelSocket to WS frame serialization).
    ws_rx: mpsc::UnboundedReceiver<WsSendItem>,
    /// Buffered read data that's accumulated across reads (dynamically grown).
    read_buf: Vec<u8>,
    /// Serialized WS frames queued for writing.
    outbox: VecDeque<Vec<u8>>,
    /// Whether a write SQE is currently in-flight.
    writing: bool,
    /// Whether the connection is shutting down (close frame sent).
    closing: bool,
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

    /// Return a connection slot to the pool. Safe to call on uninitialized (None) slots.
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

    pub fn next_generation(&mut self) -> u32 {
        let g = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        g
    }

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

    /// Return a buffer to the pool. Caller must ensure `idx` is not already free.
    pub fn free(&mut self, idx: u16) {
        debug_assert!(
            !self.free_list.contains(&idx),
            "BufferPool double-free of index {idx}"
        );
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

    #[cfg(test)]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[cfg(test)]
    pub fn total_len(&self) -> usize {
        self.data.len()
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Build iovecs suitable for io_uring register_buffers.
    /// Each iovec maps one chunk in the buffer pool.
    pub fn iovecs(&self) -> Vec<libc::iovec> {
        let count = self.data.len() / self.chunk_size;
        let base = self.data.as_ptr();
        (0..count)
            .map(|i| libc::iovec {
                iov_base: unsafe { base.add(i * self.chunk_size) as *mut libc::c_void },
                iov_len: self.chunk_size,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// submit helpers
// ---------------------------------------------------------------------------

/// Submit a multishot accept — a single SQE that produces one CQE per accepted connection.
/// Re-arm only when the kernel signals the multishot is done (no IORING_CQE_F_MORE flag).
#[inline]
fn submit_accept_multi(ring: &mut IoUring, listen_fd: RawFd) {
    let entry = opcode::AcceptMulti::new(types::Fd(listen_fd))
        .build()
        .user_data(encode_user_data(STATE_ACCEPTING, 0, 0));
    unsafe {
        ring.submission().push(&entry).ok();
    }
}

/// Set TCP_CORK on a raw fd (best-effort). Coalesces small writes into fewer packets.
#[inline]
fn set_tcp_cork(fd: RawFd, enable: bool) {
    unsafe {
        let flag: libc::c_int = if enable { 1 } else { 0 };
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_CORK,
            &flag as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Submit a read — uses ReadFixed (zero-copy kernel I/O) when buffers are registered,
/// falls back to plain Read otherwise.
#[inline]
fn submit_read_impl(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *mut u8,
    len: u32,
    offset: usize,
    slot_idx: u32,
    generation: u32,
    state: u8,
    buf_pool_idx: u16,
    bufs_registered: bool,
) {
    let ptr = buf_ptr.wrapping_add(offset);
    let remaining = len.saturating_sub(offset as u32);
    let entry = if bufs_registered {
        opcode::ReadFixed::new(types::Fd(fd), ptr, remaining, buf_pool_idx)
            .build()
    } else {
        opcode::Read::new(types::Fd(fd), ptr, remaining)
            .build()
    };
    unsafe {
        ring.submission().push(&entry.user_data(encode_user_data(state, slot_idx, generation))).ok();
    }
}

#[inline]
fn submit_read(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *mut u8,
    len: u32,
    offset: usize,
    slot_idx: u32,
    generation: u32,
    buf_pool_idx: u16,
    bufs_registered: bool,
) {
    submit_read_impl(ring, fd, buf_ptr, len, offset, slot_idx, generation, STATE_READING, buf_pool_idx, bufs_registered);
}

#[inline]
fn submit_ws_read(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *mut u8,
    len: u32,
    offset: usize,
    slot_idx: u32,
    generation: u32,
    buf_pool_idx: u16,
    bufs_registered: bool,
) {
    submit_read_impl(ring, fd, buf_ptr, len, offset, slot_idx, generation, STATE_WS_READING, buf_pool_idx, bufs_registered);
}

#[inline]
fn submit_write_with_state(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *const u8,
    len: u32,
    slot_idx: u32,
    generation: u32,
    state: u8,
) {
    let entry = opcode::Write::new(types::Fd(fd), buf_ptr, len)
        .build()
        .user_data(encode_user_data(state, slot_idx, generation));
    unsafe {
        ring.submission().push(&entry).ok();
    }
}

#[inline]
fn submit_write(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *const u8,
    len: u32,
    slot_idx: u32,
    generation: u32,
) {
    submit_write_with_state(ring, fd, buf_ptr, len, slot_idx, generation, STATE_WRITING);
}

#[inline]
fn submit_ws_write(
    ring: &mut IoUring,
    fd: RawFd,
    buf_ptr: *const u8,
    len: u32,
    slot_idx: u32,
    generation: u32,
) {
    submit_write_with_state(ring, fd, buf_ptr, len, slot_idx, generation, STATE_WS_WRITING);
}

#[inline]
fn submit_close(ring: &mut IoUring, fd: RawFd, slot_idx: u32, generation: u32) {
    let entry = opcode::Close::new(types::Fd(fd))
        .build()
        .user_data(encode_user_data(STATE_CLOSING, slot_idx, generation));
    unsafe {
        ring.submission().push(&entry).ok();
    }
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
    if let Some(slot) = conn_pool.get_mut(slot_idx) {
        buf_pool.free(slot.read_buf_idx);
        slot.write_buf.clear();
        slot.write_buf.extend_from_slice(response);
        slot.write_offset = 0;
        slot.keep_alive = false;
        let ptr = slot.write_buf.as_ptr();
        let len = slot.write_buf.len() as u32;
        submit_write(ring, fd, ptr, len, slot_idx, generation);
    } else {
        submit_close(ring, fd, slot_idx, generation);
    }
}

// ---------------------------------------------------------------------------
// PendingRequest — collects parsed requests for batched execution
// ---------------------------------------------------------------------------

struct PendingRequest {
    slot_idx: u32,
    generation: u32,
    fd: RawFd,
    keep_alive: bool,
}

/// A WebSocket message to dispatch through the channel GenServer.
struct WsPendingDispatch {
    slot_idx: u32,
    generation: u32,
    text: String,
}

/// Synchronously write a 503 response and close the fd.
/// Used when the server is out of connection or buffer slots.
#[inline]
fn reject_and_close(fd: RawFd) {
    unsafe {
        libc::write(
            fd,
            http_parse::RESPONSE_503.as_ptr() as *const libc::c_void,
            http_parse::RESPONSE_503.len(),
        );
        libc::close(fd);
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
    ws_config: Option<&WsConfig>,
    bufs_registered: bool,
) {
    let peer_addr = default_peer_addr();
    let mut accept_backoff_ms: u64 = 100;
    // Pre-clone runtime once if WS is enabled; avoid per-request Arc::clone otherwise.
    let runtime_for_conn: Option<Arc<Runtime>> = if ws_config.is_some() {
        Some(Arc::clone(runtime))
    } else {
        None
    };
    submit_accept_multi(ring, listen_fd);

    // Reusable vectors to avoid per-iteration allocation.
    let mut pending_meta: Vec<PendingRequest> = Vec::with_capacity(64);
    let mut pending_conns: Vec<Conn> = Vec::with_capacity(64);
    let mut results: Vec<Conn> = Vec::with_capacity(64);
    let mut cqes: Vec<io_uring::cqueue::Entry> = Vec::with_capacity(256);
    let mut ws_dispatches: Vec<WsPendingDispatch> = Vec::with_capacity(64);
    // Track slots that have active WS connections for outbox polling.
    let mut ws_active_slots: Vec<(u32, u32)> = Vec::with_capacity(256); // (slot_idx, generation)

    // Conn object pool — recycle Conn structs to reuse HeaderMap/HashMap capacity.
    let mut conn_recycle: Vec<Conn> = Vec::with_capacity(CONN_POOL_CAP);

    // Write buffer pool — recycle Vec<u8> for ConnSlot write buffers.
    let mut write_buf_pool: Vec<Vec<u8>> = Vec::with_capacity(WRITE_BUF_POOL_CAP);

    loop {
        if let Err(e) = ring.submit_and_wait(1) {
            tracing::error!("io_uring submit_and_wait error: {}", e);
            continue;
        }

        pending_meta.clear();
        pending_conns.clear();
        ws_dispatches.clear();

        // Timespec for EMFILE backoff — must outlive the CQE processing loop
        // so the io_uring SQE pointer remains valid until submit_and_wait.
        let mut emfile_timespec: Option<types::Timespec>;

        // Drain CQEs into reusable vec (avoids allocation after first iteration).
        cqes.clear();
        cqes.extend(ring.completion());

        for cqe in cqes.iter() {
            let (state, slot_idx, generation) = decode_user_data(cqe.user_data());
            let result = cqe.result();

            match state {
                STATE_ACCEPTING => {
                    // Multishot accept: IORING_CQE_F_MORE means more CQEs coming.
                    // Only re-arm when multishot is exhausted (no MORE flag).
                    let multishot_active = io_uring::cqueue::more(cqe.flags());

                    if result < 0 {
                        let errno = -result;
                        if errno == libc::EMFILE || errno == libc::ENFILE {
                            tracing::warn!("accept error (fd exhaustion, backoff {}ms): {}", accept_backoff_ms, std::io::Error::from_raw_os_error(errno));
                            emfile_timespec = Some(types::Timespec::from(
                                std::time::Duration::from_millis(accept_backoff_ms),
                            ));
                            let timeout_entry = opcode::Timeout::new(emfile_timespec.as_ref().unwrap() as *const types::Timespec)
                                .build()
                                .user_data(encode_user_data(STATE_ACCEPTING, 0, 0));
                            unsafe {
                                ring.submission().push(&timeout_entry).ok();
                            }
                            accept_backoff_ms = (accept_backoff_ms * 2).min(1000);
                        } else {
                            tracing::warn!("accept error: {}", std::io::Error::from_raw_os_error(errno));
                            if !multishot_active {
                                submit_accept_multi(ring, listen_fd);
                            }
                        }
                        continue;
                    }
                    accept_backoff_ms = 100;
                    let new_fd = result;

                    set_tcp_nodelay(new_fd);

                    let slot_idx = match conn_pool.alloc() {
                        Some(idx) => idx,
                        None => {
                            reject_and_close(new_fd);
                            if !multishot_active {
                                submit_accept_multi(ring, listen_fd);
                            }
                            continue;
                        }
                    };

                    let buf_idx = match buf_pool.alloc() {
                        Some(idx) => idx,
                        None => {
                            conn_pool.free(slot_idx);
                            reject_and_close(new_fd);
                            if !multishot_active {
                                submit_accept_multi(ring, listen_fd);
                            }
                            continue;
                        }
                    };

                    let generation = conn_pool.next_generation();

                    let write_buf = if let Some(mut wb) = write_buf_pool.pop() {
                        wb.clear();
                        wb
                    } else {
                        Vec::with_capacity(512)
                    };

                    conn_pool.insert(
                        slot_idx,
                        ConnSlot {
                            fd: new_fd,
                            generation,
                            read_buf_idx: buf_idx,
                            read_len: 0,
                            write_buf,
                            write_offset: 0,
                            keep_alive: true,
                            ws: None,
                        },
                    );

                    let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                    let chunk_len = buf_pool.chunk_size() as u32;
                    submit_read(ring, new_fd, buf_ptr, chunk_len, 0, slot_idx, generation, buf_idx, bufs_registered);
                    // Multishot: no re-arm needed when MORE flag is set.
                    if !multishot_active {
                        submit_accept_multi(ring, listen_fd);
                    }
                }

                STATE_READING => {
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
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

                    // Try to reuse a recycled Conn, or parse into a new one.
                    // Both paths now detect ws_key, so recycling is always safe.
                    let parse_result = if !conn_recycle.is_empty() {
                        let mut recycled = conn_recycle.pop().unwrap();
                        match http_parse::try_parse_into_conn(&mut recycled, buf, body_limit, peer_addr) {
                            Ok(Some(parsed)) => {
                                // Check for WebSocket upgrade on recycled path
                                if let (Some(ws_key), Some(_wsc)) =
                                    (parsed.ws_key, ws_config)
                                {
                                    conn_recycle.push(recycled);
                                    let slot = conn_pool.get_mut(slot_idx).unwrap();
                                    buf_pool.free(slot.read_buf_idx);
                                    slot.write_buf.clear();
                                    http_parse::serialize_ws_accept_response(&ws_key, &mut slot.write_buf);
                                    slot.write_offset = 0;
                                    slot.keep_alive = false;

                                    let ptr = slot.write_buf.as_ptr();
                                    let len = slot.write_buf.len() as u32;
                                    let entry = opcode::Write::new(types::Fd(fd), ptr, len)
                                        .build()
                                        .user_data(encode_user_data(STATE_WS_UPGRADING, slot_idx, generation));
                                    unsafe {
                                        ring.submission().push(&entry).ok();
                                    }
                                    None
                                } else {
                                    // Runtime is preserved across reset() — no Arc::clone needed.
                                    let conn = recycled;
                                    pending_meta.push(PendingRequest {
                                        slot_idx,
                                        generation,
                                        fd,
                                        keep_alive: parsed.keep_alive,
                                    });
                                    pending_conns.push(conn);
                                    None // consumed successfully
                                }
                            }
                            Ok(None) => {
                                conn_recycle.push(recycled);
                                Some(Ok(None))
                            }
                            Err(e) => {
                                conn_recycle.push(recycled);
                                Some(Err(e))
                            }
                        }
                    } else {
                        match http_parse::try_parse_request(buf, body_limit, peer_addr) {
                            Ok(Some(parsed)) => {
                                // Check for WebSocket upgrade
                                if let (Some(ws_key), Some(_wsc)) =
                                    (parsed.ws_key, ws_config)
                                {
                                    let slot = conn_pool.get_mut(slot_idx).unwrap();
                                    buf_pool.free(slot.read_buf_idx);
                                    slot.write_buf.clear();
                                    http_parse::serialize_ws_accept_response(&ws_key, &mut slot.write_buf);
                                    slot.write_offset = 0;
                                    slot.keep_alive = false;

                                    let ptr = slot.write_buf.as_ptr();
                                    let len = slot.write_buf.len() as u32;
                                    let entry = opcode::Write::new(types::Fd(fd), ptr, len)
                                        .build()
                                        .user_data(encode_user_data(STATE_WS_UPGRADING, slot_idx, generation));
                                    unsafe {
                                        ring.submission().push(&entry).ok();
                                    }
                                    None
                                } else {
                                    let conn = if let Some(ref rt) = runtime_for_conn {
                                        parsed.conn.with_runtime(Arc::clone(rt))
                                    } else {
                                        parsed.conn
                                    };
                                    pending_meta.push(PendingRequest {
                                        slot_idx,
                                        generation,
                                        fd,
                                        keep_alive: parsed.keep_alive,
                                    });
                                    pending_conns.push(conn);
                                    None
                                }
                            }
                            other => Some(other),
                        }
                    };

                    match parse_result {
                        None => {
                            // Successfully parsed — already pushed to pending.
                        }
                        Some(Ok(None)) => {
                            if read_len >= chunk_size {
                                write_static_error_and_close(
                                    ring, fd, http_parse::RESPONSE_413,
                                    slot_idx, generation, conn_pool, buf_pool,
                                );
                            } else {
                                let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                                submit_read(
                                    ring, fd, buf_ptr, chunk_size as u32,
                                    read_len, slot_idx, generation, buf_idx,
                                    bufs_registered,
                                );
                            }
                        }
                        Some(Ok(Some(_))) => unreachable!(),
                        Some(Err(ParseError::BodyTooLarge)) => {
                            write_static_error_and_close(
                                ring, fd, http_parse::RESPONSE_413,
                                slot_idx, generation, conn_pool, buf_pool,
                            );
                        }
                        Some(Err(ParseError::InvalidRequest)) => {
                            write_static_error_and_close(
                                ring, fd, http_parse::RESPONSE_400,
                                slot_idx, generation, conn_pool, buf_pool,
                            );
                        }
                    }
                }

                STATE_WS_UPGRADING => {
                    // 101 response has been written. Initialize WS state.
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
                        if let Some(slot) = conn_pool.get(slot_idx) {
                            let fd = slot.fd;
                            submit_close(ring, fd, slot_idx, generation);
                        }
                        continue;
                    }

                    let slot = match conn_pool.get_mut(slot_idx) {
                        Some(s) => s,
                        None => continue,
                    };
                    slot.write_offset += result as usize;

                    // Partial write of 101 — continue writing.
                    if slot.write_offset < slot.write_buf.len() {
                        let fd = slot.fd;
                        let ptr = unsafe { slot.write_buf.as_ptr().add(slot.write_offset) };
                        let remaining = (slot.write_buf.len() - slot.write_offset) as u32;
                        let entry = opcode::Write::new(types::Fd(fd), ptr, remaining)
                            .build()
                            .user_data(encode_user_data(STATE_WS_UPGRADING, slot_idx, generation));
                        unsafe {
                            ring.submission().push(&entry).ok();
                        }
                        continue;
                    }

                    // 101 fully written. Initialize WS state.
                    let wsc = ws_config.unwrap();
                    let (tx, rx) = mpsc::unbounded_channel::<WsSendItem>();

                    // Start GenServer for this connection
                    let gen_pid = tokio_rt.block_on(async {
                        let server = mahalo_channel::socket::ChannelConnectionServer::new(
                            Arc::clone(&wsc.channel_router),
                            wsc.pubsub.clone(),
                            tx.clone(),
                            Arc::clone(runtime),
                        );
                        gen_server::start(runtime, server, rmpv::Value::Nil).await
                    });

                    slot.ws = Some(Box::new(WsState {
                        gen_pid,
                        ws_rx: rx,
                        read_buf: Vec::with_capacity(8192),
                        outbox: VecDeque::new(),
                        writing: false,
                        closing: false,
                    }));

                    // Re-use the read buffer for WS reads. Allocate a new one from the pool.
                    let new_buf_idx = match buf_pool.alloc() {
                        Some(idx) => idx,
                        None => {
                            // No buffer available — close the connection.
                            let fd = slot.fd;
                            runtime.kill(gen_pid);
                            submit_close(ring, fd, slot_idx, generation);
                            continue;
                        }
                    };
                    slot.read_buf_idx = new_buf_idx;
                    slot.read_len = 0;

                    let fd = slot.fd;
                    let buf_ptr = buf_pool.slice_mut(new_buf_idx).as_mut_ptr();
                    let chunk_len = buf_pool.chunk_size() as u32;
                    submit_ws_read(ring, fd, buf_ptr, chunk_len, 0, slot_idx, generation, new_buf_idx, bufs_registered);

                    ws_active_slots.push((slot_idx, generation));
                }

                STATE_WS_READING => {
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
                        // Client disconnected. Kill GenServer and close.
                        if let Some(slot) = conn_pool.get_mut(slot_idx) {
                            if let Some(ws) = slot.ws.take() {
                                runtime.kill(ws.gen_pid);
                            }
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
                    let fd = slot.fd;

                    // Append new data to WS read buffer.
                    let ws = slot.ws.as_mut().unwrap();
                    let buf_idx = slot.read_buf_idx;
                    let new_data = &buf_pool.slice(buf_idx)[..n];
                    ws.read_buf.extend_from_slice(new_data);

                    // Parse all complete frames.
                    loop {
                        match ws_parse::try_parse_frame(&ws.read_buf) {
                            Ok(Some((frame, consumed))) => {
                                match frame.opcode {
                                    ws_parse::OPCODE_TEXT => {
                                        if let Ok(text) = String::from_utf8(frame.payload) {
                                            ws_dispatches.push(WsPendingDispatch {
                                                slot_idx,
                                                generation,
                                                text,
                                            });
                                        }
                                    }
                                    ws_parse::OPCODE_BINARY => {
                                        // Treat binary as text for Phoenix compat.
                                        if let Ok(text) = String::from_utf8(frame.payload) {
                                            ws_dispatches.push(WsPendingDispatch {
                                                slot_idx,
                                                generation,
                                                text,
                                            });
                                        }
                                    }
                                    ws_parse::OPCODE_PING => {
                                        // Reply with pong (same payload).
                                        let mut pong_buf = Vec::new();
                                        ws_parse::serialize_frame(ws_parse::OPCODE_PONG, &frame.payload, &mut pong_buf);
                                        ws.outbox.push_back(pong_buf);
                                    }
                                    ws_parse::OPCODE_PONG => {
                                        // Ignore unsolicited pongs.
                                    }
                                    ws_parse::OPCODE_CLOSE => {
                                        if !ws.closing {
                                            ws.closing = true;
                                            // Echo close frame back.
                                            let code = if frame.payload.len() >= 2 {
                                                u16::from_be_bytes([frame.payload[0], frame.payload[1]])
                                            } else {
                                                1000
                                            };
                                            let reason = if frame.payload.len() > 2 {
                                                &frame.payload[2..]
                                            } else {
                                                b""
                                            };
                                            let mut close_buf = Vec::new();
                                            ws_parse::serialize_close_frame(code, reason, &mut close_buf);
                                            ws.outbox.push_back(close_buf);
                                        }
                                    }
                                    _ => {
                                        // Unknown opcode — protocol error close.
                                        ws.closing = true;
                                        let mut close_buf = Vec::new();
                                        ws_parse::serialize_close_frame(1002, b"unsupported opcode", &mut close_buf);
                                        ws.outbox.push_back(close_buf);
                                    }
                                }
                                // Remove consumed bytes from read_buf.
                                ws.read_buf.drain(..consumed);
                            }
                            Ok(None) => break, // Need more data.
                            Err(_) => {
                                // Protocol error — close connection.
                                ws.closing = true;
                                let mut close_buf = Vec::new();
                                ws_parse::serialize_close_frame(1002, b"protocol error", &mut close_buf);
                                ws.outbox.push_back(close_buf);
                                break;
                            }
                        }
                    }

                    // Re-arm the read if not closing.
                    let ws = slot.ws.as_ref().unwrap();
                    if !ws.closing || !ws.outbox.is_empty() {
                        let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                        let chunk_len = buf_pool.chunk_size() as u32;
                        submit_ws_read(ring, fd, buf_ptr, chunk_len, 0, slot_idx, generation, buf_idx, bufs_registered);
                    }
                }

                STATE_WRITING => {
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
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
                        let ptr = unsafe { slot.write_buf.as_ptr().add(slot.write_offset) };
                        let remaining = (slot.write_buf.len() - slot.write_offset) as u32;
                        submit_write(ring, fd, ptr, remaining, slot_idx, generation);
                    } else if slot.keep_alive {
                        // Uncork to flush the coalesced response.
                        set_tcp_cork(fd, false);
                        let buf_idx = slot.read_buf_idx;
                        slot.read_len = 0;
                        slot.write_buf.clear();
                        slot.write_offset = 0;

                        let buf_ptr = buf_pool.slice_mut(buf_idx).as_mut_ptr();
                        let chunk_len = buf_pool.chunk_size() as u32;
                        submit_read(ring, fd, buf_ptr, chunk_len, 0, slot_idx, generation, buf_idx, bufs_registered);
                    } else {
                        set_tcp_cork(fd, false);
                        buf_pool.free(slot.read_buf_idx);
                        submit_close(ring, fd, slot_idx, generation);
                    }
                }

                STATE_WS_WRITING => {
                    let slot_gen = conn_pool.get(slot_idx).map(|s| s.generation);
                    if slot_gen != Some(generation & 0x00FF_FFFF) {
                        continue;
                    }

                    if result <= 0 {
                        // Write error — close.
                        if let Some(slot) = conn_pool.get_mut(slot_idx) {
                            if let Some(ws) = slot.ws.take() {
                                runtime.kill(ws.gen_pid);
                            }
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
                        // Partial write — continue.
                        let ptr = unsafe { slot.write_buf.as_ptr().add(slot.write_offset) };
                        let remaining = (slot.write_buf.len() - slot.write_offset) as u32;
                        submit_ws_write(ring, fd, ptr, remaining, slot_idx, generation);
                    } else {
                        // Write complete. Check for more outbox data.
                        let ws = slot.ws.as_mut().unwrap();
                        ws.writing = false;

                        if ws.closing && ws.outbox.is_empty() {
                            // Close frame sent. Kill GenServer and close fd.
                            let gen_pid = ws.gen_pid;
                            slot.ws = None;
                            runtime.kill(gen_pid);
                            buf_pool.free(slot.read_buf_idx);
                            ws_active_slots.retain(|&(si, _)| si != slot_idx);
                            submit_close(ring, fd, slot_idx, generation);
                        }
                        // If outbox has more, it'll be flushed in the outbox flush phase below.
                    }
                }

                STATE_CLOSING => {
                    // Remove from ws_active_slots if present.
                    ws_active_slots.retain(|&(si, _)| si != slot_idx);
                    // Recycle the write buffer before freeing the slot.
                    if let Some(slot) = conn_pool.get_mut(slot_idx) {
                        if write_buf_pool.len() < WRITE_BUF_POOL_CAP {
                            let mut wb = std::mem::take(&mut slot.write_buf);
                            wb.clear();
                            write_buf_pool.push(wb);
                        }
                    }
                    conn_pool.free(slot_idx);
                }

                _ => {}
            }
        }

        // ---- Batched HTTP handler execution ----
        if !pending_conns.is_empty() {
            results.clear();
            let r = &mut results;
            let pc = &mut pending_conns;
            // Single block_on call for all pending requests — amortizes
            // tokio runtime enter/leave overhead across the batch.
            tokio_rt.block_on(async {
                for conn in pc.drain(..) {
                    r.push(crate::handler::execute_request(conn, router, error_handler, after_plugs).await);
                }
            });

            for (meta, conn) in pending_meta.drain(..).zip(results.drain(..)) {
                let slot = match conn_pool.get_mut(meta.slot_idx) {
                    Some(s) => s,
                    None => {
                        // Recycle the Conn even if slot is gone.
                        if conn_recycle.len() < CONN_POOL_CAP {
                            conn_recycle.push(conn);
                        }
                        continue;
                    }
                };
                http_parse::serialize_response_into(&conn, meta.keep_alive, &mut slot.write_buf);
                slot.write_offset = 0;
                slot.keep_alive = meta.keep_alive;

                // Recycle the Conn for future requests.
                if conn_recycle.len() < CONN_POOL_CAP {
                    conn_recycle.push(conn);
                }

                // TCP_CORK coalesces header + body into a single packet.
                set_tcp_cork(meta.fd, true);
                let ptr = slot.write_buf.as_ptr();
                let len = slot.write_buf.len() as u32;
                submit_write(ring, meta.fd, ptr, len, meta.slot_idx, meta.generation);
            }
        }

        // ---- Batched WebSocket dispatch ----
        if !ws_dispatches.is_empty() {
            tokio_rt.block_on(async {
                let mut ack_rxs = Vec::new();
                for dispatch in ws_dispatches.drain(..) {
                    let slot = match conn_pool.get(dispatch.slot_idx) {
                        Some(s) if s.generation == (dispatch.generation & 0x00FF_FFFF) => s,
                        _ => continue,
                    };
                    if let Some(ws) = &slot.ws {
                        let cast_val = rmpv::Value::String(rmpv::Utf8String::from(dispatch.text));
                        if let Ok(rx) = gen_server::cast_from_runtime_ack(runtime, ws.gen_pid, cast_val).await {
                            ack_rxs.push(rx);
                        }
                    }
                }
                if !ack_rxs.is_empty() {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        futures::future::join_all(ack_rxs),
                    ).await;
                }
            });
        }

        // ---- Drain WS outbound channels + flush outboxes ----
        // For each active WS connection, drain the mpsc receiver and serialize frames.
        for &(si, sg) in ws_active_slots.iter() {
            let slot = match conn_pool.get_mut(si) {
                Some(s) if s.generation == (sg & 0x00FF_FFFF) => s,
                _ => continue,
            };
            let fd = slot.fd;
            let ws = match slot.ws.as_mut() {
                Some(ws) => ws,
                None => continue,
            };

            // Drain ChannelSocket mpsc → serialize to WS text frames.
            while let Ok(json) = ws.ws_rx.try_recv() {
                let mut frame_buf = Vec::new();
                ws_parse::serialize_frame(ws_parse::OPCODE_TEXT, json.as_bytes(), &mut frame_buf);
                ws.outbox.push_back(frame_buf);
            }

            // Flush outbox if not currently writing.
            if !ws.writing && !ws.outbox.is_empty() {
                slot.write_buf.clear();
                slot.write_offset = 0;
                // Concatenate all outbox frames into write_buf.
                for frame_data in ws.outbox.drain(..) {
                    slot.write_buf.extend_from_slice(&frame_data);
                }
                ws.writing = true;
                let ptr = slot.write_buf.as_ptr();
                let len = slot.write_buf.len() as u32;
                submit_ws_write(ring, fd, ptr, len, si, sg);
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
        let data = encode_user_data(STATE_ACCEPTING, 1, 0x01FF_FFFF);
        let (_, _, g) = decode_user_data(data);
        assert_eq!(g, 0x00FF_FFFF);
    }

    #[test]
    fn connection_pool_alloc_free() {
        let mut pool = ConnectionPool::new(3);
        let a = pool.alloc().unwrap();
        let b = pool.alloc().unwrap();
        let c = pool.alloc().unwrap();
        assert!(pool.alloc().is_none());

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
                ws: None,
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
                ws: None,
            },
        );
        assert!(pool.get(idx).is_some());
        pool.free(idx);
        assert!(pool.get(idx).is_none());
    }

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

        pool.slice_mut(a)[0] = 0xAA;
        pool.slice_mut(a)[127] = 0xBB;
        pool.slice_mut(b)[0] = 0xCC;

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

    #[test]
    fn buffer_pool_iovecs_count_and_layout() {
        let pool = BufferPool::new(4, 128);
        let iovecs = pool.iovecs();
        assert_eq!(iovecs.len(), 4);
        let base = pool.as_ptr();
        for (i, iov) in iovecs.iter().enumerate() {
            assert_eq!(iov.iov_len, 128);
            assert_eq!(iov.iov_base as *const u8, unsafe { base.add(i * 128) });
        }
    }
}
