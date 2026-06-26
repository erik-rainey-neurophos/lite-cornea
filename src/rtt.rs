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
use std::io::{self, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};

use crate::{memory, FastModelIris};

// Iris memory space used elsewhere in cornea for CPU-side accesses.
const SPACE: u64 = 0;

// SEGGER_RTT_CB layout (32-bit target): char acID[16]; i32 MaxNumUpBuffers;
// i32 MaxNumDownBuffers; SEGGER_RTT_BUFFER aUp[]; SEGGER_RTT_BUFFER aDown[];
const CB_MAX_UP_OFFSET: u64 = 16;
const CB_BUFFERS_OFFSET: u64 = 24;
// SEGGER_RTT_BUFFER (32-bit): const char* sName; char* pBuffer; u32 SizeOfBuffer;
// u32 WrOff; u32 RdOff; u32 Flags;
const BUFFER_STRIDE: u64 = 24;
const BUF_PBUFFER: u64 = 4;
const BUF_SIZE: u64 = 8;
const BUF_WROFF: u64 = 12;
const BUF_RDOFF: u64 = 16;

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
    servers: Vec<Server>,
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
        let max_up = read_u32(iris, inst, cb + CB_MAX_UP_OFFSET)?;
        Ok((cb, max_up))
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

    fn pump(&mut self, iris: &mut FastModelIris, inst: u32) {
        // Find the control block lazily if a server started before `rtt start`.
        if self.control_block.is_none() {
            if let Some((addr, size, id)) = self.search.clone() {
                self.control_block = self
                    .find_control_block(iris, inst, addr, size, id.as_bytes())
                    .ok()
                    .flatten();
            }
        }
        let cb = match self.control_block {
            Some(cb) => cb,
            None => return,
        };

        // Read each served channel once, then distribute (a channel may feed more
        // than one server). Reading advances the target's RdOff.
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
            if let Some(bytes) = data.get(&srv.channel) {
                // Keep clients that accept the write (or merely would-block);
                // drop only those that error out (closed connection).
                srv.clients.retain_mut(|c| match c.write_all(bytes) {
                    Ok(()) => true,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => true,
                    Err(_) => false,
                });
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
