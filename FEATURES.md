# Features & Target Parity

`cornea` bridges an ARM Fast Models Iris server to GDB. It provides two GDB stub
targets, selected by the CPU instance being debugged:

| Module | Arch | Type | Typical core |
| --- | --- | --- | --- |
| `src/gdb/t32.rs` | ARMv7-M (`Armv7mArch`) | 32-bit | Cortex-M (e.g. `component.cpuss.cpu`, Cortex-M4F) |
| `src/gdb/a64.rs` | ARMv8-A (`Armv8aArch`) | 64-bit | Cortex-A |

Both are single-threaded GDB targets backed by the same Iris RPC layer
(`resource`, `memory`, `breakpoint`, `simulation`, …) in `src/lib.rs`.

## Parity matrix

| Feature | t32 (ARMv7-M) | a64 (ARMv8-A) | Notes |
| --- | :---: | :---: | --- |
| Register read (`read_registers`) | ✅ | ✅ | Maps Iris resources → GDB register block |
| Register write (`write_registers`) | ✅ | ✅ | Skips resources Iris reports read-only |
| Single-register access (`read_register`/`write_register`) | ✅ | ✅ | Targeted `p`/`P` packets; read-only write → NonFatal error |
| Memory read (`read_addrs`) | ✅ | ✅ | See *space resolution* below |
| Memory write (`write_addrs`) | ✅ | ✅ | Shared `memory::write` + `pack_le` path |
| Resume: step & continue | ✅ | ✅ | Polls `simulation_time` until halt |
| GDB interrupt (Ctrl-C) | ✅ | ✅ | Stops the sim, returns `GdbInterrupt` |
| RTT pump during `continue` (bidirectional) | ✅ | ✅ | `rtt.poll`/`flush` in the resume loop: up channels → TCP, client input → down channels |
| Software breakpoints | ✅ | ✅ | Delegate to hardware breakpoints |
| Hardware breakpoints | ✅ | ✅ | Via `breakpoint::code` |
| Hardware watchpoints | ✅ | ✅ | read/write/access; reported as `StopReason::Watch` |
| Watch-trigger event stream | ✅ | ✅ | Both subscribe to `IRIS_BREAKPOINT_HIT` to map a data-bp hit to a watch stop |
| `monitor reset` (session-robust) | ✅ | ✅ | Reports Iris errors without dropping the session |
| `monitor halt` | ✅ | ✅ | Stops the sim (OpenOCD-style); Ctrl-C is the async equivalent |
| `monitor rtt …` (setup/start/server) | ✅ | ✅ | OpenOCD-style RTT control; `server start` opens a bidirectional TCP channel |

## RTT (bidirectional)

RTT is host-side polling of a SEGGER RTT control block in target RAM over Iris
memory access — there is no trace hardware. `src/rtt.rs` finds the control block,
drains the up buffers (target → host) out to loopback TCP servers, and writes
bytes received from those sockets into the matching down buffer (host → target).
The same channel index serves both directions, so a connected client is a full
terminal. Driven from gdb over the proxy, OpenOCD-style:

    monitor rtt setup <addr> <size> [id]     # search region + control-block ID
    monitor rtt start                        # locate the control block
    monitor rtt server start <port> <chan>   # loopback TCP for up+down of <chan>
    monitor rtt server stop <port>
    continue                                 # RTT only flows while the target runs

Then attach any TCP client (`nc localhost <port>`, an RTT terminal): its output
is the up channel; keystrokes go to the down channel. Caveats:

- Data only moves during `continue` (the pump runs in the resume loop, throttled).
  Input typed at a halted prompt is buffered and flushed on the next resume.
- Input requires the firmware to read its down buffer (`SEGGER_RTT_Read`/`GetKey`).
- Servers bind `127.0.0.1` only; SSH-forward the port for a remote FVP.
- A channel with no down buffer silently drops input (bounds-checked); the
  per-channel host → target backlog is capped at 64 KiB.

## Architecture-specific by design (not gaps)

These differ because the architectures differ; they are intentional, not missing
parity:

- **Register file.** t32 exposes ARMv7-M registers (`R0`–`R15`, `XPSR`); a64
  exposes ARMv8-A registers (`X0`–`X30`, `SP`, `PC`, `XPSR`/`CPSR`). The
  name↔index mapping lives in each module's `regnum_of`/`block_index` helpers.
- **Memory space resolution.** Cortex-M has a flat address map, so t32 always
  uses Iris space `0`. ARMv8-A exposes several spaces (translation regimes), so
  a64 resolves the active space from the `PC_MEMSPACE` resource
  (`pc_memspace` helper, shared by `read_addrs`/`write_addrs`).
- **Breakpoint/watchpoint storage.** t32 stores a single Iris breakpoint id per
  address (flat space 0); a64 stores a `Vec` of ids, one per memory space.

## Known gaps & follow-ups

The two stubs are now at full functional parity; the only open items are
verification, not missing features:

- **a64 verification on real hardware** — the a64-specific paths (`write_addrs`,
  watchpoints, single-register access) mirror the t32 implementations but have
  only been exercised against a Cortex-M4F (t32) model. They still need
  confirmation against a real ARMv8-A target.
