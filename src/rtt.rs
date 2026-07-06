//! SEGGER RTT over the Iris debug connection.
//!
//! RTT is purely a convention over target RAM: the firmware publishes a control
//! block (a 16-byte ID string followed by ring-buffer descriptors), and the host
//! reads the "up" ring buffers (target -> host) and writes the "down" ones. The
//! Iris server gives us full memory read/write, which is all RTT physically
//! needs -- there is no trace hardware involved. This module finds the control
//! block, drains the up channels, and fans the bytes out to OpenOCD-style TCP
//! servers (`rtt server start <port> <channel>`).
//!
//! It is driven from the gdb stub's resume loop: the target only produces output
//! while it runs, so `poll()` must be called repeatedly during `continue`, on the
//! same thread that owns the (single) Iris connection.

use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::{memory, FastModelIris};

// Iris memory space used elsewhere in cornea for CPU-side accesses.
const SPACE: u64 = 0;

// SEGGER_RTT_CB layout (32-bit target): char acID[16]; i32 MaxNumUpBuffers;
// i32 MaxNumDownBuffers; SEGGER_RTT_BUFFER aUp[]; SEGGER_RTT_BUFFER aDown[];
const CB_MAX_UP_OFFSET: u64 = 16;
const CB_MAX_DOWN_OFFSET: u64 = 20;
const CB_BUFFERS_OFFSET: u64 = 24;
// SEGGER_RTT_BUFFER (32-bit): const char* sName; char* pBuffer; u32 SizeOfBuffer;
// u32 WrOff; u32 RdOff; u32 Flags;
const BUFFER_STRIDE: u64 = 24;
const BUF_PBUFFER: u64 = 4;
const BUF_SIZE: u64 = 8;
const BUF_WROFF: u64 = 12;
const BUF_RDOFF: u64 = 16;

// Cap on host->target bytes buffered per channel while the target is not
// draining its down buffer, so a paste at a halted prompt cannot grow the
// backlog without bound.
const MAX_PENDING: usize = 64 * 1024;

fn read_bytes(iris: &mut FastModelIris, inst: u32, addr: u64, n: u64) -> io::Result<Vec<u8>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    // memory_read returns `count` * `byteWidth` bytes packed little-endian into
    // 64-bit words; byteWidth=1, count=n yields n bytes (ceil(n/8) words).
    let res = memory::read(iris, inst, SPACE, addr, 1, n)?;
    let mut bytes: Vec<u8> = res.data.into_iter().flat_map(|u| u.to_le_bytes()).collect();
    bytes.truncate(n as usize);
    if (bytes.len() as u64) < n {
        return Err(io::Error::new(ErrorKind::UnexpectedEof, "short memory read"));
    }
    Ok(bytes)
}

fn read_u32(iris: &mut FastModelIris, inst: u32, addr: u64) -> io::Result<u32> {
    let b = read_bytes(iris, inst, addr, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn write_u32(iris: &mut FastModelIris, inst: u32, addr: u64, val: u32) -> io::Result<()> {
    // byteWidth=4, count=1: write the 32-bit value as one little-endian access.
    memory::write(iris, inst, SPACE, addr, 4, 1, vec![val as u64])?;
    Ok(())
}

fn write_bytes(iris: &mut FastModelIris, inst: u32, addr: u64, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    // byteWidth=1, count=len: mirror of read_bytes and the memory-write subcommand.
    memory::write(iris, inst, SPACE, addr, 1, bytes.len() as u64, memory::pack_le(bytes))?;
    Ok(())
}

struct Server {
    port: u16,
    channel: u32,
    listener: TcpListener,
    clients: Vec<TcpStream>,
}

#[derive(Default)]
pub struct Rtt {
    // (search address, search length, control-block ID string).
    search: Option<(u64, u64, String)>,
    control_block: Option<u64>,
    // Buffer counts from the control-block header, needed to address the down
    // buffers (which follow the up buffers) and to bounds-check channels.
    num_up: u32,
    num_down: u32,
    servers: Vec<Server>,
    // Per-channel host->target backlog awaiting free space in a down buffer.
    pending_down: HashMap<u32, Vec<u8>>,
    poll_counter: u32,
}

impl Rtt {
    /// `rtt setup <addr> <size> [id]` -- where and what to search for.
    pub fn setup(&mut self, addr: u64, size: u64, id: String) {
        self.search = Some((addr, size, id));
        self.control_block = None;
    }

    /// `rtt start` -- locate the control block by scanning the search region.
    /// Returns the control-block address and the number of up channels.
    pub fn start(&mut self, iris: &mut FastModelIris, inst: u32) -> io::Result<(u64, u32)> {
        let (addr, size, id) = self
            .search
            .clone()
            .ok_or_else(|| io::Error::new(ErrorKind::Other, "rtt setup not run"))?;
        let cb = self
            .find_control_block(iris, inst, addr, size, id.as_bytes())?
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "RTT control block not found"))?;
        self.control_block = Some(cb);
        self.num_up = read_u32(iris, inst, cb + CB_MAX_UP_OFFSET)?;
        self.num_down = read_u32(iris, inst, cb + CB_MAX_DOWN_OFFSET)?;
        Ok((cb, self.num_up))
    }

    /// `rtt server start <port> <channel>`.
    pub fn server_start(&mut self, port: u16, channel: u32) -> io::Result<()> {
        if self.servers.iter().any(|s| s.port == port) {
            return Err(io::Error::new(ErrorKind::AddrInUse, "port already serving"));
        }
        // Bind loopback only: cornea runs host-local (you connect via localhost,
        // or SSH-forward the port on a remote farm), so there is no reason to
        // expose the channel on the network.
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        listener.set_nonblocking(true)?;
        self.servers.push(Server {
            port,
            channel,
            listener,
            clients: Vec::new(),
        });
        Ok(())
    }

    /// `rtt server stop <port>`. Returns whether a server was removed.
    pub fn server_stop(&mut self, port: u16) -> bool {
        let before = self.servers.len();
        self.servers.retain(|s| s.port != port);
        self.servers.len() != before
    }

    /// Scan `[addr, addr+size)` for the control-block ID. Reads in overlapping
    /// windows so a marker straddling a window boundary is still found, and stops
    /// at the first unreadable address (the search range routinely overshoots the
    /// mapped RAM region).
    fn find_control_block(
        &self,
        iris: &mut FastModelIris,
        inst: u32,
        addr: u64,
        size: u64,
        id: &[u8],
    ) -> io::Result<Option<u64>> {
        if id.is_empty() {
            return Ok(None);
        }
        const WINDOW: u64 = 4096;
        let overlap = (id.len() as u64) - 1;
        let end = addr.saturating_add(size);
        let mut pos = addr;
        while pos < end {
            let want = WINDOW.min(end - pos);
            let chunk = match read_bytes(iris, inst, pos, want) {
                Ok(c) => c,
                Err(_) => break, // unmapped: nothing more to scan
            };
            if let Some(off) = chunk.windows(id.len()).position(|w| w == id) {
                return Ok(Some(pos + off as u64));
            }
            if want < WINDOW {
                break;
            }
            pos += WINDOW - overlap;
        }
        Ok(None)
    }

    /// Throttled drain for the resume loop: cheap no-op until a server is running,
    /// and only does real work every few calls so it does not flood Iris. Call
    /// repeatedly while the target runs.
    pub fn poll(&mut self, iris: &mut FastModelIris, inst: u32) {
        if self.servers.is_empty() {
            return;
        }
        self.poll_counter = self.poll_counter.wrapping_add(1);
        if self.poll_counter % 4 != 0 {
            return;
        }
        self.pump(iris, inst);
    }

    /// Unthrottled drain, for a final flush once the target has stopped so the
    /// last bytes before a breakpoint are not left behind.
    pub fn flush(&mut self, iris: &mut FastModelIris, inst: u32) {
        if self.servers.is_empty() {
            return;
        }
        self.pump(iris, inst);
    }

    /// Resolve and cache the control-block address plus the up/down buffer
    /// counts, so a server that started before `rtt start` still finds the block
    /// lazily on the first pump.
    fn resolve_cb(&mut self, iris: &mut FastModelIris, inst: u32) -> Option<u64> {
        if self.control_block.is_none() {
            if let Some((addr, size, id)) = self.search.clone() {
                if let Some(cb) = self
                    .find_control_block(iris, inst, addr, size, id.as_bytes())
                    .ok()
                    .flatten()
                {
                    self.control_block = Some(cb);
                    self.num_up = read_u32(iris, inst, cb + CB_MAX_UP_OFFSET).unwrap_or(0);
                    self.num_down = read_u32(iris, inst, cb + CB_MAX_DOWN_OFFSET).unwrap_or(0);
                }
            }
        }
        self.control_block
    }

    fn pump(&mut self, iris: &mut FastModelIris, inst: u32) {
        let cb = match self.resolve_cb(iris, inst) {
            Some(cb) => cb,
            None => return,
        };

        // Up channels (target -> host): read each served channel once, then
        // distribute (a channel may feed more than one server). Reading advances
        // the target's RdOff.
        let mut channels: Vec<u32> = self.servers.iter().map(|s| s.channel).collect();
        channels.sort_unstable();
        channels.dedup();
        let mut data: HashMap<u32, Vec<u8>> = HashMap::new();
        for ch in channels {
            if let Ok(bytes) = drain_up_channel(iris, inst, cb, ch) {
                if !bytes.is_empty() {
                    data.insert(ch, bytes);
                }
            }
        }

        // Service each server socket: accept, read client input (host -> target),
        // then push up-channel output back out.
        let mut inbound: HashMap<u32, Vec<u8>> = HashMap::new();
        for srv in &mut self.servers {
            // Accept any pending connections (non-blocking).
            loop {
                match srv.listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(true);
                        srv.clients.push(stream);
                    }
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            let out = data.get(&srv.channel);
            let mut got: Vec<u8> = Vec::new();
            srv.clients.retain_mut(|c| {
                // Drain whatever the client has sent. A clean EOF (Ok(0)) or a hard
                // error drops the client; WouldBlock just means nothing more now.
                let mut tmp = [0u8; 512];
                loop {
                    match c.read(&mut tmp) {
                        Ok(0) => return false,
                        Ok(n) => got.extend_from_slice(&tmp[..n]),
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(_) => return false,
                    }
                }
                // Forward up-channel output. Keep clients that accept the write or
                // merely would-block; drop only those that error out.
                if let Some(bytes) = out {
                    match c.write_all(bytes) {
                        Ok(()) => {}
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(_) => return false,
                    }
                }
                true
            });
            if !got.is_empty() {
                inbound.entry(srv.channel).or_default().extend(got);
            }
        }

        // Down channels (host -> target): queue new input, then push as much as
        // the target's down buffer will currently accept.
        for (ch, bytes) in inbound {
            let queue = self.pending_down.entry(ch).or_default();
            queue.extend(bytes);
            if queue.len() > MAX_PENDING {
                let excess = queue.len() - MAX_PENDING;
                queue.drain(..excess);
            }
        }
        let num_up = self.num_up;
        let num_down = self.num_down;
        for (ch, queue) in self.pending_down.iter_mut() {
            if queue.is_empty() || *ch >= num_down {
                continue;
            }
            if let Ok(written) = fill_down_channel(iris, inst, cb, num_up, *ch, queue.as_slice()) {
                if written > 0 {
                    queue.drain(..written);
                }
            }
        }
    }
}

/// Read all pending bytes from up channel `ch` and advance the target's RdOff so
/// the ring keeps flowing (otherwise a full buffer makes the target drop output).
fn drain_up_channel(
    iris: &mut FastModelIris,
    inst: u32,
    cb: u64,
    ch: u32,
) -> io::Result<Vec<u8>> {
    let base = cb + CB_BUFFERS_OFFSET + (ch as u64) * BUFFER_STRIDE;
    let pbuffer = read_u32(iris, inst, base + BUF_PBUFFER)? as u64;
    let size = read_u32(iris, inst, base + BUF_SIZE)?;
    let wroff = read_u32(iris, inst, base + BUF_WROFF)?;
    let rdoff = read_u32(iris, inst, base + BUF_RDOFF)?;
    if size == 0 || pbuffer == 0 || wroff == rdoff || wroff >= size || rdoff >= size {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    if wroff > rdoff {
        out = read_bytes(iris, inst, pbuffer + rdoff as u64, (wroff - rdoff) as u64)?;
    } else {
        // Wrapped: [rdoff, size) then [0, wroff).
        out.extend(read_bytes(iris, inst, pbuffer + rdoff as u64, (size - rdoff) as u64)?);
        out.extend(read_bytes(iris, inst, pbuffer, wroff as u64)?);
    }
    write_u32(iris, inst, base + BUF_RDOFF, wroff)?;
    Ok(out)
}

/// Write as many queued bytes as fit into down channel `ch` (host -> target),
/// handling wrap and keeping one slot reserved so a full ring stays
/// distinguishable from an empty one, then advance the target's WrOff. Returns
/// the number of bytes accepted (the caller drops those from its backlog).
fn fill_down_channel(
    iris: &mut FastModelIris,
    inst: u32,
    cb: u64,
    num_up: u32,
    ch: u32,
    data: &[u8],
) -> io::Result<usize> {
    if data.is_empty() {
        return Ok(0);
    }
    // Down buffers sit immediately after the up buffers in the control block.
    let base = cb + CB_BUFFERS_OFFSET + ((num_up + ch) as u64) * BUFFER_STRIDE;
    let pbuffer = read_u32(iris, inst, base + BUF_PBUFFER)? as u64;
    let size = read_u32(iris, inst, base + BUF_SIZE)?;
    let wroff = read_u32(iris, inst, base + BUF_WROFF)?;
    let rdoff = read_u32(iris, inst, base + BUF_RDOFF)?;
    if size == 0 || pbuffer == 0 || wroff >= size || rdoff >= size {
        return Ok(0);
    }
    // Host owns WrOff, target owns RdOff. Available space with one slot reserved,
    // matching SEGGER_RTT's down-buffer write.
    let free = if rdoff <= wroff {
        size - 1 - wroff + rdoff
    } else {
        rdoff - wroff - 1
    };
    if free == 0 {
        return Ok(0);
    }
    let n = (data.len() as u32).min(free);
    // First span runs to the end of the ring; the remainder wraps to the front.
    let first = (size - wroff).min(n);
    write_bytes(iris, inst, pbuffer + wroff as u64, &data[..first as usize])?;
    if n > first {
        write_bytes(iris, inst, pbuffer, &data[first as usize..n as usize])?;
    }
    write_u32(iris, inst, base + BUF_WROFF, (wroff + n) % size)?;
    Ok(n as usize)
}
