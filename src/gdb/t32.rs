use std::collections::hash_map::{Entry, HashMap};
use std::convert::TryInto;
use std::io::{Error as IOError, Read, Stdin, Stdout, Write};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::spawn;

use gdbstub::arch::{Arch, RegId, Registers};
use gdbstub::target::ext::base::singlethread::{SingleThreadOps, StopReason};
use gdbstub::target::ext::base::{
    BaseOps, ResumeAction, SingleRegisterAccess, SingleRegisterAccessOps,
};
#[allow(unused)]
use gdbstub::target::ext::breakpoints::{
    Breakpoints, BreakpointsOps, HwBreakpoint, HwBreakpointOps, HwWatchpoint, HwWatchpointOps,
    SwBreakpoint, SwBreakpointOps, WatchKind,
};
use gdbstub::target::ext::monitor_cmd::{ConsoleOutput, MonitorCmd, MonitorCmdOps};
use gdbstub::target::{Target, TargetResult};
use gdbstub::{outputln, Connection};

use serde::Deserialize;

use crate::{
    breakpoint, event, event_stream, instance_registry, memory, resource, simulation,
    simulation_time, step, FastModelIris,
};

#[derive(Debug, Deserialize)]
struct WatchTrigger {
    #[serde(rename = "ACCESS_RW")]
    kind: String,
    #[serde(rename = "ACCESS_ADDR")]
    addr: u64,
    #[serde(rename = "BPT_ID")]
    id: u64,
}

pub struct IrisGdbStub<'i> {
    pub iris: &'i mut FastModelIris,
    pub instance_id: u32,
    sim: u32,
    breakpoints: HashMap<u32, u64>,
    watchpoints: HashMap<u32, u64>,
    last_watch_trigger: Arc<Mutex<Option<WatchTrigger>>>,
    rtt: crate::rtt::Rtt,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GuestState {
    pub regs: [u32; 26],
}

impl<'i> IrisGdbStub<'i> {
    pub fn from_instance(iris: &'i mut FastModelIris, instance_id: u32) -> std::io::Result<Self> {
        let sim = instance_registry::get_instance_by_name(
            iris,
            "framework.SimulationEngine".to_string(),
        )?;
        // Subscribe to breakpoint-hit events so a data watchpoint trigger can be
        // turned into a gdb StopReason::Watch (the event carries which bpt fired
        // and the accessed address). Mirrors the a64 stub.
        let source = event::source(iris, instance_id, "IRIS_BREAKPOINT_HIT".to_string())?;
        let last_watch_trigger = Arc::new(Mutex::new(None));
        let _stream = event_stream::create(
            iris,
            Some(instance_id),
            false,
            iris.inst_id.unwrap(),
            source.id,
            false,
            true,
        )?;
        let cb_last_watch_trigger = last_watch_trigger.clone();
        iris.register_callback(
            "ec_IRIS_BREAKPOINT_HIT".to_string(),
            Box::new(move |mut params| {
                if let Ok(ref mut trigger) = cb_last_watch_trigger.try_lock() {
                    if let Some(watch_trigger) = params
                        .as_object_mut()
                        .and_then(|p| p.get_mut("fields"))
                        .and_then(|f| serde_json::value::from_value(f.take()).ok())
                    {
                        **trigger = Some(watch_trigger);
                    }
                }
                Ok(())
            }),
        );
        Ok(Self {
            iris,
            instance_id,
            breakpoints: HashMap::new(),
            watchpoints: HashMap::new(),
            last_watch_trigger,
            sim: sim.id,
            rtt: crate::rtt::Rtt::default(),
        })
    }
}

impl Registers for GuestState {
    type ProgramCounter = u32;
    fn pc(&self) -> u32 {
        self.regs[15]
    }
    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for (num, reg) in self.regs.iter().enumerate() {
            for byte in reg.to_le_bytes().iter() {
                write_byte(Some(*byte));
            }
            // Registers above 16 and below 24 are assumed to be 96 bit by gdb.
            // So we pad them
            if num >= 16 && num < 24 {
                for _ in 0..8 {
                    write_byte(Some(0));
                }
            }
        }
    }
    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() % 4 != 0 {
            return Err(());
        }
        let mut regs = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()));
        for reg in &mut self.regs {
            *reg = regs.next().ok_or(())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Register {
    R0,
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
    R7,
    R8,
    R9,
    R10,
    R11,
    R12,
    SP,
    LR,
    PC,
    XPSR,
}

impl RegId for Register {
    fn from_raw_id(id: usize) -> Option<(Self, usize)> {
        use Register::*;
        Some(match id {
            0 => R0,
            1 => R1,
            2 => R2,
            3 => R3,
            4 => R4,
            5 => R5,
            6 => R6,
            7 => R7,
            8 => R8,
            9 => R9,
            10 => R10,
            11 => R11,
            12 => R12,
            13 => SP,
            14 => LR,
            15 => PC,
            25 => XPSR,
            _ => return None,
        })
        .map(|r| (r, 0))
    }
}

/// Map an Iris resource name to its index in the gdb register block, or `None`
/// if the resource is not one of the registers gdb tracks. Single source of
/// truth for the name<->index mapping used by all four register accessors.
fn regnum_of(name: &str) -> Option<usize> {
    Some(match name {
        "R0" => 0,
        "R1" => 1,
        "R2" => 2,
        "R3" => 3,
        "R4" => 4,
        "R5" => 5,
        "R6" => 6,
        "R7" => 7,
        "R8" => 8,
        "R9" => 9,
        "R10" => 10,
        "R11" => 11,
        "R12" => 12,
        "R13" => 13,
        "R14" => 14,
        "R15" => 15,
        "XPSR" => 25,
        _ => return None,
    })
}

/// Map a gdb register id to its index in the gdb register block. Note the gdb
/// ids name the banked registers (SP/LR/PC) while Iris exposes them as
/// R13/R14/R15, so this is distinct from [`regnum_of`].
fn block_index(reg: &Register) -> usize {
    use Register::*;
    match reg {
        R0 => 0,
        R1 => 1,
        R2 => 2,
        R3 => 3,
        R4 => 4,
        R5 => 5,
        R6 => 6,
        R7 => 7,
        R8 => 8,
        R9 => 9,
        R10 => 10,
        R11 => 11,
        R12 => 12,
        SP => 13,
        LR => 14,
        PC => 15,
        XPSR => 25,
    }
}

impl<'i> Target for IrisGdbStub<'i> {
    type Arch = Armv7mArch;
    type Error = ();
    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    fn breakpoints(&mut self) -> Option<BreakpointsOps<'_, Self>> {
        Some(self)
    }

    fn monitor_cmd(&mut self) -> Option<MonitorCmdOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadOps for IrisGdbStub<'_> {
    fn read_registers(&mut self, regs: &mut GuestState) -> TargetResult<(), Self> {
        for res in
            resource::get_list(&mut self.iris, self.instance_id, None, None).map_err(|_| ())?
        {
            let regnum = match regnum_of(&res.name) {
                Some(regnum) => regnum,
                None => continue,
            };
            let val =
                resource::read(&mut self.iris, self.instance_id, vec![res.id]).map_err(|_| ())?;
            if !val.data.is_empty() {
                regs.regs[regnum] = val.data[0] as u32
            }
        }
        Ok(())
    }

    fn read_addrs(&mut self, start_addr: u32, data: &mut [u8]) -> TargetResult<(), Self> {
        let mem = memory::read(
            &mut self.iris,
            self.instance_id,
            0,
            start_addr as u64,
            1,
            data.len() as u64,
        )
        .map_err(|_| ())?;
        for (offset, byte) in mem
            .data
            .into_iter()
            .map(|u| u.to_le_bytes())
            .flatten()
            .enumerate()
        {
            if data.len() > offset {
                data[offset] = byte;
            }
        }
        Ok(())
    }

    fn write_addrs(&mut self, start_addr: u32, data: &[u8]) -> TargetResult<(), Self> {
        memory::write(
            self.iris,
            self.instance_id,
            0,
            start_addr as u64,
            1,
            data.len() as u64,
            memory::pack_le(data),
        )
        .map_err(|_| ())?;
        Ok(())
    }
    fn write_registers(&mut self, regs: &GuestState) -> TargetResult<(), Self> {
        for res in
            resource::get_list(&mut self.iris, self.instance_id, None, None).map_err(|_| ())?
        {
            let regnum = match regnum_of(&res.name) {
                Some(regnum) => regnum,
                None => continue,
            };
            // Skip resources Iris reports as read-only.
            if matches!(res.rw_mode.as_deref(), Some(mode) if !mode.contains('w')) {
                continue;
            }
            resource::write(
                &mut self.iris,
                self.instance_id,
                vec![res.id],
                vec![regs.regs[regnum] as u64],
            )
            .map_err(|_| ())?;
        }
        Ok(())
    }

    fn single_register_access(&mut self) -> Option<SingleRegisterAccessOps<'_, (), Self>> {
        Some(self)
    }

    fn resume(
        &mut self,
        act: ResumeAction,
        intr: gdbstub::target::ext::base::GdbInterrupt<'_>,
    ) -> Result<StopReason<u32>, ()> {
        let mut interrupt = intr.no_async();
        if act == ResumeAction::Step {
            step::setup(self.iris, self.instance_id, 1, step::Unit::Instruction).map_err(|_| ())?
        }
        if act == ResumeAction::Step || act == ResumeAction::Continue {
            simulation_time::run(self.iris, self.sim).map_err(|_| ())?;
            while simulation_time::get(self.iris, self.sim)
                .map_err(|_| ())?
                .running
            {
                // The target only produces RTT output while it runs, so pump the
                // up channels here (no-op unless an `rtt server` is active).
                self.rtt.poll(&mut *self.iris, self.instance_id);
                if interrupt.pending() {
                    self.rtt.flush(&mut *self.iris, self.instance_id);
                    simulation_time::stop(self.iris, self.sim).map_err(|_| ())?;
                    return Ok(StopReason::GdbInterrupt);
                }
                // Throttle the run-state poll: an unthrottled tight loop hammers
                // the Iris IPC right after the async simulationTime_run and
                // intermittently desyncs it, so the proxy reports a bogus stop and
                // gdb/model disagree on run state (the "Cannot execute ... target is
                // running" seen under scripted continue/step). A small sleep both
                // rate-limits the IPC and lets the run take effect. Kept short so
                // instruction single-stepping stays responsive. Matches a64.rs.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            // Drain anything emitted just before the stop.
            self.rtt.flush(&mut *self.iris, self.instance_id);
            if act == ResumeAction::Step {
                return Ok(StopReason::DoneStep);
            } else {
                if let Ok(mut locked) = self.last_watch_trigger.try_lock() {
                    if let Some(trigger) = locked.take() {
                        let kind = match trigger.kind.as_str() {
                            "r" => WatchKind::Read,
                            "w" => WatchKind::Write,
                            "rw" => WatchKind::ReadWrite,
                            _ => return Ok(StopReason::HwBreak),
                        };
                        let addr = self
                            .watchpoints
                            .iter()
                            .find_map(|(k, v)| if *v == trigger.id { Some(*k) } else { None });
                        let addr = addr.unwrap_or(trigger.addr as u32);
                        return Ok(StopReason::Watch { kind, addr });
                    }
                }
                return Ok(StopReason::HwBreak);
            }
        }
        Err(())
    }
}

impl<'i> SingleRegisterAccess<()> for IrisGdbStub<'i> {
    fn read_register(
        &mut self,
        _tid: (),
        reg_id: Register,
        dst: &mut [u8],
    ) -> TargetResult<(), Self> {
        let target = block_index(&reg_id);
        for res in
            resource::get_list(&mut self.iris, self.instance_id, None, None).map_err(|_| ())?
        {
            if regnum_of(&res.name) != Some(target) {
                continue;
            }
            let val =
                resource::read(&mut self.iris, self.instance_id, vec![res.id]).map_err(|_| ())?;
            if let Some(&value) = val.data.first() {
                let bytes = (value as u32).to_le_bytes();
                let n = dst.len().min(bytes.len());
                dst[..n].copy_from_slice(&bytes[..n]);
            }
            break;
        }
        Ok(())
    }

    fn write_register(&mut self, _tid: (), reg_id: Register, val: &[u8]) -> TargetResult<(), Self> {
        let target = block_index(&reg_id);
        let mut buf = [0u8; 4];
        let n = val.len().min(buf.len());
        buf[..n].copy_from_slice(&val[..n]);
        let value = u32::from_le_bytes(buf) as u64;
        for res in
            resource::get_list(&mut self.iris, self.instance_id, None, None).map_err(|_| ())?
        {
            if regnum_of(&res.name) != Some(target) {
                continue;
            }
            // Reject writes to resources Iris reports as read-only (NonFatal, so
            // gdb reports the failure without dropping the session).
            if matches!(res.rw_mode.as_deref(), Some(mode) if !mode.contains('w')) {
                return Err(().into());
            }
            resource::write(&mut self.iris, self.instance_id, vec![res.id], vec![value])
                .map_err(|_| ())?;
            break;
        }
        Ok(())
    }
}

impl<'i> Breakpoints for IrisGdbStub<'i> {
    fn hw_breakpoint(&mut self) -> Option<HwBreakpointOps<'_, Self>> {
        Some(self)
    }

    fn hw_watchpoint(&mut self) -> Option<HwWatchpointOps<'_, Self>> {
        Some(self)
    }

    fn sw_breakpoint(&mut self) -> Option<SwBreakpointOps<'_, Self>> {
        Some(self)
    }
}
impl<'i> SwBreakpoint for IrisGdbStub<'i> {
    fn add_sw_breakpoint(
        &mut self,
        addr: <Self::Arch as Arch>::Usize,
        k: <Self::Arch as Arch>::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        self.add_hw_breakpoint(addr, k)
    }

    fn remove_sw_breakpoint(
        &mut self,
        addr: <Self::Arch as Arch>::Usize,
        k: <Self::Arch as Arch>::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        self.remove_hw_breakpoint(addr, k)
    }
}

impl<'i> HwBreakpoint for IrisGdbStub<'i> {
    fn add_hw_breakpoint(
        &mut self,
        addr: <Self::Arch as Arch>::Usize,
        _: <Self::Arch as Arch>::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        if self.breakpoints.contains_key(&addr) {
            return Ok(true);
        }
        if let Ok(id) = breakpoint::code(self.iris, self.instance_id, addr as u64, None, 0, false) {
            self.breakpoints.insert(addr, id);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn remove_hw_breakpoint(
        &mut self,
        addr: <Self::Arch as Arch>::Usize,
        _: <Self::Arch as Arch>::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        if let Entry::Occupied(ent) = self.breakpoints.entry(addr) {
            if let Ok(()) = breakpoint::delete(self.iris, self.instance_id, *ent.get()) {
                let _ = ent.remove_entry();
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Ok(true)
        }
    }
}

fn kind_to_str(kind: WatchKind) -> String {
    match kind {
        WatchKind::Read => "r",
        WatchKind::Write => "w",
        WatchKind::ReadWrite => "rw",
    }
    .to_string()
}

impl<'i> HwWatchpoint for IrisGdbStub<'i> {
    fn add_hw_watchpoint(
        &mut self,
        addr: <Self::Arch as Arch>::Usize,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        if self.watchpoints.contains_key(&addr) {
            return Ok(true);
        }
        // Cortex-M is a flat single-space map, so set one data breakpoint in
        // space 0 (unlike a64, which sets one per memory space).
        match breakpoint::set(
            self.iris,
            self.instance_id,
            addr as u64,
            Some(kind_to_str(kind)),
            None,
            Some(0),
            crate::breakpoint::Type::Data,
            false,
            false,
        ) {
            Ok(id) => {
                self.watchpoints.insert(addr, id);
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }
    fn remove_hw_watchpoint(
        &mut self,
        addr: <Self::Arch as Arch>::Usize,
        _kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        if let Entry::Occupied(ent) = self.watchpoints.entry(addr) {
            if let Ok(()) = breakpoint::delete(self.iris, self.instance_id, *ent.get()) {
                let _ = ent.remove_entry();
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Ok(true)
        }
    }
}

/// Parse a `0x`-prefixed hex or plain decimal unsigned integer.
pub(crate) fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => s.parse::<u64>().ok(),
    }
}

impl<'i> MonitorCmd for IrisGdbStub<'i> {
    fn handle_monitor_cmd(&mut self, cmd: &[u8], mut out: ConsoleOutput<'_>) -> Result<(), ()> {
        let cmd = String::from_utf8_lossy(cmd);
        let tokens: Vec<&str> = cmd.split_whitespace().collect();
        match tokens.as_slice() {
            [] => {}
            // "reset" / "reset halt": reset and leave the core halted (the sim is
            // stopped after reset, which is the halted state for a debug session).
            // A SystemC EVS cannot re-elaborate, so Iris returns an error there --
            // report it but keep the session alive (start the model halted with
            // --iris-wait instead of resetting).
            ["reset", ..] => match simulation::reset(self.iris, self.sim, false) {
                Ok(_) => {
                    let _ = simulation::wait(self.iris, self.sim);
                    outputln!(out, "reset");
                }
                Err(e) => outputln!(out, "reset unavailable: {}", e),
            },
            // "halt": stop a free-running target (OpenOCD-style). In this
            // synchronous bridge the sim only advances inside `resume`, so at the
            // monitor prompt it is usually already stopped and this is a no-op;
            // Ctrl-C is the async interrupt that halts while running.
            ["halt", ..] => match simulation_time::stop(self.iris, self.sim) {
                Ok(_) => outputln!(out, "halted"),
                Err(e) => outputln!(out, "halt unavailable: {}", e),
            },
            // OpenOCD-style RTT control. The firmware .gdb script drives these.
            ["rtt", "setup", addr, size, id_rest @ ..] => {
                match (parse_u64(addr), parse_u64(size)) {
                    (Some(addr), Some(size)) => {
                        let id = if id_rest.is_empty() {
                            "SEGGER RTT".to_string()
                        } else {
                            id_rest.join(" ").trim_matches('"').to_string()
                        };
                        self.rtt.setup(addr, size, id);
                        outputln!(out, "rtt: search {:#x}..{:#x}", addr, addr + size);
                    }
                    _ => outputln!(out, "rtt setup <addr> <size> [id]: bad address/size"),
                }
            }
            ["rtt", "start"] => match self.rtt.start(&mut *self.iris, self.instance_id) {
                Ok((cb, n)) => outputln!(out, "rtt: control block {:#x}, {} up channel(s)", cb, n),
                Err(e) => outputln!(out, "rtt start: {}", e),
            },
            ["rtt", "server", "start", port, channel] => {
                match (port.parse::<u16>().ok(), parse_u64(channel)) {
                    (Some(port), Some(ch)) => {
                        match self.rtt.server_start(port, ch as u32) {
                            Ok(()) => outputln!(out, "rtt: channel {} -> 127.0.0.1:{}", ch, port),
                            Err(e) => outputln!(out, "rtt server start: {}", e),
                        }
                    }
                    _ => outputln!(out, "rtt server start <port> <channel>: bad args"),
                }
            }
            ["rtt", "server", "stop", port] => match port.parse::<u16>() {
                Ok(port) => {
                    let removed = self.rtt.server_stop(port);
                    let state = if removed { "stopped" } else { "not running" };
                    outputln!(out, "rtt: server :{} {}", port, state);
                }
                Err(_) => outputln!(out, "rtt server stop <port>: bad port"),
            },
            _ => outputln!(out, "Monitor command '{}' not supported", cmd.trim()),
        }
        Ok(())
    }
}

pub enum Armv7mArch {}
impl Arch for Armv7mArch {
    type Usize = u32;
    type Registers = GuestState;
    type RegId = Register;
    type BreakpointKind = usize;
}

pub struct GdbOverPipe {
    rx: Receiver<Result<u8, IOError>>,
    write: Stdout,
}

impl<'a> GdbOverPipe {
    pub fn new(read: Stdin, write: Stdout) -> Self {
        let (tx, rx) = channel();
        spawn(move || {
            let mut byte = [0u8];
            let mut read = read;
            loop {
                match read.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => tx.send(Ok(byte[0])).unwrap(),
                    Err(error) => tx.send(Err(error)).unwrap(),
                }
            }
        });
        Self { rx, write }
    }
}

impl Connection for GdbOverPipe {
    type Error = IOError;
    fn write(&mut self, byte: u8) -> Result<(), Self::Error> {
        let outbuf = [byte; 1];
        self.write.write(&outbuf)?;
        self.write.flush()?;
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.write.flush()
    }
    fn read(&mut self) -> Result<u8, Self::Error> {
        self.rx
            .recv()
            .map_err(|_| std::io::ErrorKind::ConnectionReset)?
    }
    fn peek(&mut self) -> Result<Option<u8>, Self::Error> {
        match self.rx.try_recv() {
            Ok(res) => res.map(Some),
            Err(_) => Ok(None),
        }
    }
}
