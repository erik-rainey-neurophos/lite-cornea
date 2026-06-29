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
| RTT pump during `continue` | ✅ | ✅ | `rtt.poll`/`flush` in the resume loop |
| Software breakpoints | ✅ | ✅ | Delegate to hardware breakpoints |
| Hardware breakpoints | ✅ | ✅ | Via `breakpoint::code` |
| Hardware watchpoints | ✅ | ✅ | read/write/access; reported as `StopReason::Watch` |
| Watch-trigger event stream | ✅ | ✅ | Both subscribe to `IRIS_BREAKPOINT_HIT` to map a data-bp hit to a watch stop |
| `monitor reset` (session-robust) | ✅ | ✅ | Reports Iris errors without dropping the session |
| `monitor halt` | ✅ | ✅ | Stops the sim (OpenOCD-style); Ctrl-C is the async equivalent |
| `monitor rtt …` (setup/start/server) | ✅ | ✅ | OpenOCD-style RTT control |

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
