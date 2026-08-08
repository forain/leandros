//! meminfo — ask the buddy allocator how much physical memory is left.
//!
//! LeandrOS has no `/proc/meminfo`, so until now the only way to learn how much
//! memory the guest had left was to add a kernel print and rebuild. The answer
//! was already reachable: `sys_sysinfo` fills `totalram`/`freeram` straight from
//! `mm::buddy::total_pages()`/`free_pages()`, and nothing in the image called
//! it.
//!
//! One line per invocation, so a session script can sample it at every phase
//! boundary and what gets read is a series rather than a point. A single
//! reading cannot tell "the guest is nearly out" from "the guest was nearly out
//! for a moment", and an allocation failure is a question about the trough.
//!
//!     meminfo [label]
//!     MEMINFO <label> total=2147483648 free=1234567168 used=912916480 usedpct=42 uptime=123
//!
//! `used` is `total - free` in bytes, i.e. everything the buddy allocator has
//! handed out — kernel, page cache and every process together — not one
//! process's RSS.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::syscall::syscall1;
use leandros_libc::{write, STDOUT_FILENO};

// Linux's own numbers, which is what kernel/src/syscall.rs uses.
#[cfg(target_arch = "aarch64")]
const SYS_SYSINFO: usize = 179;
#[cfg(target_arch = "x86_64")]
const SYS_SYSINFO: usize = 99;

// struct sysinfo, the 112-byte kernel layout sys_sysinfo writes. Only the three
// fields below are populated by this kernel; the rest are zeroed.
const OFF_UPTIME: usize = 0; // i64, seconds
const OFF_TOTALRAM: usize = 32; // u64, bytes (mem_unit is 1)
const OFF_FREERAM: usize = 40; // u64, bytes
const OFF_PROCS: usize = 80; // u16, live processes

/// The buffer is `[u64; 14]` rather than `[u8; 112]` so that it is 8-aligned:
/// the kernel writes `u64`s into it through raw pointers, and an unaligned
/// destination is a fault on aarch64 rather than a slow store.
#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut si = [0u64; 14];
    let rc = syscall1(SYS_SYSINFO, si.as_mut_ptr() as usize);
    if rc < 0 {
        let msg = b"MEMINFO sysinfo failed\n";
        write(STDOUT_FILENO, msg.as_ptr(), msg.len());
        return 1;
    }

    let base = si.as_ptr() as *const u8;
    let uptime = core::ptr::read_unaligned(base.add(OFF_UPTIME) as *const u64);
    let total = core::ptr::read_unaligned(base.add(OFF_TOTALRAM) as *const u64);
    let free = core::ptr::read_unaligned(base.add(OFF_FREERAM) as *const u64);
    let procs = core::ptr::read_unaligned(base.add(OFF_PROCS) as *const u16);
    let used = total.saturating_sub(free);
    // Integer percentage; `total` is never 0 on a booted guest, but a division
    // by zero here would be a fault rather than a wrong number, so it is guarded.
    let usedpct = if total == 0 { 0 } else { used * 100 / total };

    let mut out = Out::new();
    out.s(b"MEMINFO ");
    if argc > 1 && !argv.is_null() {
        let label = *argv.add(1);
        if !label.is_null() {
            let mut n = 0usize;
            while n < 64 && *label.add(n) != 0 {
                n += 1;
            }
            out.raw(label, n);
            out.s(b" ");
        }
    }
    out.s(b"total=");
    out.u(total);
    out.s(b" free=");
    out.u(free);
    out.s(b" used=");
    out.u(used);
    out.s(b" usedpct=");
    out.u(usedpct);
    out.s(b" procs=");
    out.u(procs as u64);
    out.s(b" uptime=");
    out.u(uptime);
    out.s(b"\n");
    out.flush();
    0
}

/// One `write(2)` for the whole line. The console costs ~0.19 s per newline and
/// a partial line from a second writer would interleave into it, so the line is
/// assembled first and emitted once.
struct Out {
    buf: [u8; 256],
    len: usize,
}

impl Out {
    fn new() -> Self {
        Out { buf: [0; 256], len: 0 }
    }

    fn push(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }

    fn s(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push(b);
        }
    }

    unsafe fn raw(&mut self, p: *const u8, n: usize) {
        for i in 0..n {
            self.push(*p.add(i));
        }
    }

    fn u(&mut self, mut n: u64) {
        let mut d = [0u8; 20];
        let mut i = 20usize;
        if n == 0 {
            self.push(b'0');
            return;
        }
        while n > 0 {
            i -= 1;
            d[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        for k in i..20 {
            self.push(d[k]);
        }
    }

    fn flush(&self) {
        unsafe { write(STDOUT_FILENO, self.buf.as_ptr(), self.len) };
    }
}
