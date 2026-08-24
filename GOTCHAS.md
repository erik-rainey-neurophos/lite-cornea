# GOTCHAS

## `continue` over `gdb-proxy` never returns a stop reply (open, found 2026-08-23)

`stepi` works; `continue` does not. gdb sends `$vCont;c:p1.-1`, its `wait` returns
immediately with **no stop reply**, and gdb latches the thread as running:

```
(gdb) continue
Cannot execute this command while the target is running.
Use the "interrupt" command to stop the target and then try again.
```

The session is then wedged: `detach`/`quit` hang, and both `gdb` and the
`cornea gdb-proxy` process have to be killed. `interrupt` (gdb's own suggestion)
makes it worse — a storm of `Ignoring packet error, continuing...` then
`Can't detach process.`

Not the 7cc72b7 resume-poll desync: reproduced with a binary built from ccbda2b,
which contains that throttle. Reproduced in every configuration tried:

- `arm-none-eabi-gdb -batch -x` and interactive (commands on stdin)
- model launched with `--iris-wait` (halted at reset) and free-running
- arm-none-eabi-gdb 14.2.90 (Arm toolchain 13.3.Rel1) and homebrew gdb 17.2
- with and without a `Z0` breakpoint, with and without a preceding `stepi`

Everything else on the same session is healthy: `?` answers `S05`, register and
memory reads are correct, `Z0` returns `OK`, `vCont;s` steps (PC 0x0 → 0x2d80 on
the poc1 boot ROM), and all `monitor` commands work — including the full RTT
sequence, which located the control block at `0x2000183c` with 16 up channels. A
session that never resumes detaches cleanly. Suspect the Continue arm of `resume`
in `src/gdb/t32.rs` (or its gdbstub 0.5 stop-reply mapping); the `vCont` thread
suffix `:p1.-1` arrives because the stub advertises `multiprocess+`.

Reproduce with `set debug remote 1` and read the packets around `vCont;c`.

Workarounds until fixed: `stepi` for stepping, `cornea break <full.instance>
<hex-addr>` for run-to-address (works, exits 0), or run the model free (no
`--iris-wait`) and attach only to inspect. Because RTT only pumps inside the
resume loop, a gdb-driven RTT console cannot stream while this is broken.

## Iris `PC` resource does not exist on Cortex-M

`register-read <inst> PC` matches only `PC_MEMSPACE` and prints nothing useful.
On M-class the PC is `R15`, SP is `R13` (plus `R13_MAIN`/`R13_PROCESS`), LR is
`R14`. The README's `PC` example comes from an ARMv8-A core. Related: the M4F EVS
still reports three memory spaces (`Memory`, `Physical Memory`, `Current`) even
though the t32 stub hardcodes space 0.

## `find_instance` matches a stripped name exactly, not any suffix

The fallback strips the longest prefix common to *all* instances, then requires an
exact match on the remainder — so the usable shorthand depends on the model's
shape. On the single-rooted M4F EVS (`component.cpuss…`) `cpu` works and
`cpuss.cpu` fails; on TC2 (`component.TC2…`) `css.rss.cpu` works. `break` skips
the fallback entirely and needs the fully-qualified name, failing with
`instance with this name is not registered in the instance registry`.

## Piping cornea's stdout into `head` panics

`panic = "abort"` and no SIGPIPE handling, so `cornea register-list … | head`
ends with `failed printing to stdout: Broken pipe (os error 32)` and a non-zero
exit after the wanted output. Redirect to a file or use `sed -n '1,40p'`.

## `-p/--port` must precede the subcommand

`cornea -p 7100 child-list` is valid; `cornea child-list -p 7100` is a clap error
(the flag is on the top-level parser, not the subcommand). With no `-p` the client
probes 7100 then 7101–7104 and reports only the last failure, so the
`ConnectionRefused` message names no port.

## CLI sizes are hex too

`memory-read <inst> <addr> [size]` parses **both** operands with radix 16 and no
`0x` prefix, so `memory-read … 0 16` reads 0x16 = 22 bytes. Only
`memory-write <data>` and `register-write <value>` tolerate a `0x` prefix, and
`monitor rtt setup` (a different parser inside the gdb stub) accepts `0x…`.
