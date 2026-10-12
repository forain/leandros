//! A software command streamer: the "engine" behind `i915.fake` (QEMU has no
//! Intel GPU). It walks a batch the way the hardware's command parser would —
//! through the context's PPGTT, following MI_BATCH_BUFFER_START chains — and
//! carries out the few commands whose effect userspace can observe without a
//! 3D pipeline: memory writes (MI_STORE_DATA_IMM, MI_STORE_REGISTER_MEM,
//! MI_COPY_MEM_MEM, PIPE_CONTROL post-sync writes). Every other well-formed
//! command (3D state, primitives, blits, LRIs, semaphores, ...) is skipped
//! by its length, so a real Mesa batch runs to its MI_BATCH_BUFFER_END
//! without drawing anything.
//!
//! What it proves: the UAPI's objects, VM binds and fences are wired the way
//! a GPU will see them (a store lands in the BO the VA was bound to, through
//! the real page tables). What it cannot: anything about rendering.
//!
//! Pure; the host tests in drivers/i915-tests drive it over a hash map.

#![allow(dead_code)]

/// GPU memory as the command streamer sees it: PPGTT virtual addresses.
pub trait GpuMem {
    fn read32(&self, va: u64) -> Option<u32>;
    fn write32(&mut self, va: u64, v: u32) -> bool;
    /// Value MI_STORE_REGISTER_MEM / PIPE_CONTROL timestamp writes report.
    fn timestamp(&self) -> u64 { 0 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Stats {
    pub dwords: u32,
    pub commands: u32,
    pub stores: u32,
    pub chains: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Reached MI_BATCH_BUFFER_END of the first-level batch.
    Done(Stats),
    /// The batch (or a write) touched an address with nothing behind it.
    Fault { va: u64, stats: Stats },
    /// A dword that is not a command this parser knows how to size.
    BadCommand { va: u64, dw: u32, stats: Stats },
    /// More than `limit` dwords: a loop or a runaway batch (the hang case).
    Runaway(Stats),
}

const MI: u32 = 0;
const BLT: u32 = 2;
const GFX: u32 = 3;

pub const MI_NOOP: u32 = 0x00;
pub const MI_USER_INTERRUPT: u32 = 0x02;
pub const MI_WAIT_FOR_EVENT: u32 = 0x03;
pub const MI_ARB_CHECK: u32 = 0x05;
pub const MI_REPORT_HEAD: u32 = 0x07;
pub const MI_ARB_ON_OFF: u32 = 0x08;
pub const MI_BATCH_BUFFER_END: u32 = 0x0A;
pub const MI_PREDICATE: u32 = 0x0C;
pub const MI_STORE_DATA_IMM: u32 = 0x20;
pub const MI_LOAD_REGISTER_IMM: u32 = 0x22;
pub const MI_STORE_REGISTER_MEM: u32 = 0x24;
pub const MI_COPY_MEM_MEM: u32 = 0x2E;
pub const MI_BATCH_BUFFER_START: u32 = 0x31;

/// Second-level batch bit of MI_BATCH_BUFFER_START (gen8+: bit 22).
const BBS_SECOND_LEVEL: u32 = 1 << 22;
/// RCS timestamp register (what MI_STORE_REGISTER_MEM of it should see).
const RCS_TIMESTAMP: u32 = 0x2358;

/// Length in dwords of the command whose header is `dw`, or None.
pub fn cmd_len(dw: u32) -> Option<u32> {
    match dw >> 29 {
        MI => {
            let op = (dw >> 23) & 0x3F;
            if op < 0x10 { return Some(1); }
            let mask = match op {
                MI_LOAD_REGISTER_IMM => 0xFF,
                MI_STORE_DATA_IMM => 0x3FF,
                _ => 0x3F,
            };
            Some((dw & mask) + 2)
        }
        BLT => Some((dw & 0xFF) + 2),
        GFX => {
            // PIPELINE_SELECT and 3DSTATE_VF_STATISTICS are single dwords
            // whose low bits are fields, not a length.
            match dw >> 16 {
                0x6904 | 0x780B => Some(1),
                _ => Some((dw & 0xFF) + 2),
            }
        }
        _ => None,
    }
}

fn write64(mem: &mut dyn GpuMem, va: u64, v: u64, stats: &mut Stats) -> Result<(), u64> {
    if !mem.write32(va, v as u32) { return Err(va); }
    if !mem.write32(va + 4, (v >> 32) as u32) { return Err(va + 4); }
    stats.stores += 1;
    Ok(())
}

/// Run the batch at `start` for at most `limit` dwords.
pub fn run(mem: &mut dyn GpuMem, start: u64, limit: u32) -> Outcome {
    let mut st = Stats::default();
    let mut pc = start;
    // Return address of a second-level batch (gen9 has one level).
    let mut ret: Option<u64> = None;
    loop {
        if st.dwords >= limit { return Outcome::Runaway(st); }
        let Some(dw) = mem.read32(pc) else { return Outcome::Fault { va: pc, stats: st } };
        let Some(len) = cmd_len(dw) else { return Outcome::BadCommand { va: pc, dw, stats: st } };
        let mut body = [0u32; 6];
        for i in 1..(len.min(6)) {
            match mem.read32(pc + 4 * i as u64) {
                Some(v) => body[i as usize] = v,
                None => return Outcome::Fault { va: pc + 4 * i as u64, stats: st },
            }
        }
        body[0] = dw;
        st.dwords += len;
        st.commands += 1;
        let next = pc + 4 * len as u64;
        let op = (dw >> 23) & 0x3F;
        let r: Result<(), u64> = match dw >> 29 {
            MI => match op {
                MI_BATCH_BUFFER_END => {
                    match ret.take() {
                        Some(r) => { pc = r; continue; }
                        None => return Outcome::Done(st),
                    }
                }
                MI_BATCH_BUFFER_START => {
                    let target = (body[1] as u64 | (body[2] as u64) << 32) & ((1 << 48) - 1) & !3;
                    st.chains += 1;
                    if dw & BBS_SECOND_LEVEL != 0 {
                        if ret.is_some() { return Outcome::BadCommand { va: pc, dw, stats: st }; }
                        ret = Some(next);
                    }
                    pc = target;
                    continue;
                }
                MI_STORE_DATA_IMM => {
                    // gen8: header, address lo/hi, data (1 or 2 dwords).
                    let va = (body[1] as u64 | (body[2] as u64) << 32) & ((1 << 48) - 1) & !3;
                    let mut res = Ok(());
                    for i in 3..len {
                        let v = mem.read32(pc + 4 * i as u64).unwrap_or(0);
                        if !mem.write32(va + 4 * (i - 3) as u64, v) { res = Err(va); break; }
                    }
                    if res.is_ok() { st.stores += 1; }
                    res
                }
                MI_STORE_REGISTER_MEM if len >= 4 => {
                    let va = (body[2] as u64 | (body[3] as u64) << 32) & ((1 << 48) - 1) & !3;
                    let ts = mem.timestamp();
                    let v = match body[1] & 0x7F_FFFC {
                        RCS_TIMESTAMP => ts as u32,
                        x if x == RCS_TIMESTAMP + 4 => (ts >> 32) as u32,
                        _ => 0,
                    };
                    if mem.write32(va, v) { st.stores += 1; Ok(()) } else { Err(va) }
                }
                MI_COPY_MEM_MEM if len >= 5 => {
                    let dst = (body[1] as u64 | (body[2] as u64) << 32) & ((1 << 48) - 1) & !3;
                    let src = (body[3] as u64 | (body[4] as u64) << 32) & ((1 << 48) - 1) & !3;
                    match mem.read32(src) {
                        Some(v) => if mem.write32(dst, v) { st.stores += 1; Ok(()) } else { Err(dst) },
                        None => Err(src),
                    }
                }
                _ => Ok(()),
            },
            // PIPE_CONTROL (3D pipeline 2, opcode 2, subopcode 0) with a
            // post-sync operation: dw1[15:14] = 1 write immediate, 2 depth
            // count, 3 timestamp; address in dw2/dw3, immediate in dw4/dw5.
            GFX if dw >> 16 == 0x7A00 && len >= 6 => {
                let post = (body[1] >> 14) & 3;
                let va = (body[2] as u64 | (body[3] as u64) << 32) & ((1 << 48) - 1) & !7;
                match post {
                    1 => {
                        let lo = body[4] as u64;
                        let hi = body[5] as u64;
                        write64(mem, va, lo | hi << 32, &mut st)
                    }
                    2 => write64(mem, va, 0, &mut st),
                    3 => { let ts = mem.timestamp(); write64(mem, va, ts, &mut st) }
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        };
        if let Err(va) = r { return Outcome::Fault { va, stats: st }; }
        pc = next;
    }
}
