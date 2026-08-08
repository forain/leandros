//! Serial console driver.
//!
//! x86-64:  16550A UART at COM1 (0x3F8), programmed via `out` instructions.
//! AArch64: PL011 UART at 0x0900_0000 (QEMU virt), via arch_serial_putc/init
//!          symbols exported by arch-aarch64::uart (resolved at link time).

use super::{Driver, DriverError};

const COM1: u16 = 0x3F8;

// ── Architecture-specific serial output ──────────────────────────────────────

extern "C" {
    /// Initialise the UART — arch-aarch64::uart on aarch64; unused on x86_64,
    /// where `probe` programs the 16550 directly.
    #[cfg(not(target_arch = "x86_64"))]
    fn arch_serial_init();
    /// Write one byte to the UART, waiting for the transmitter against a
    /// deadline. `kernel::arch_serial_putc` on both arches, which is
    /// `arch::putc`; the same symbol every other module in this crate uses
    /// (see pci.rs, virtio_net.rs).
    fn arch_serial_putc(c: u8);
}

// ── Driver struct ─────────────────────────────────────────────────────────────

pub struct Serial {
    pub base: u16,
}

impl Serial {
    pub const fn new() -> Self { Self { base: COM1 } }

    /// Write one byte through the arch UART primitive.
    ///
    /// NOT THE CONSOLE PATH, on either arch. Nothing constructs `Serial`, so
    /// neither this nor `probe` below has ever run. Console output goes
    ///   sys_write -> console_write_user -> serial_write_raw
    ///             -> kernel::serial_write_byte -> arch::putc
    /// and `arch::putc` is where the transmitter handshake and its deadline
    /// live. This is recorded because the x86_64 arm used to be a bare
    /// `out dx, al` with no `LSR.THRE` check, which reads exactly like a
    /// console with no flow control and was diagnosed as the cause of lost
    /// output that turned out not to be lost at all. Routing it through the
    /// same bounded primitive as every other caller in this crate leaves
    /// nothing here to misread.
    pub fn write_byte(&self, b: u8) {
        unsafe { arch_serial_putc(b); }
    }

    pub fn write_str(&self, s: &str) {
        for b in s.bytes() { self.write_byte(b); }
    }
}

impl Driver for Serial {
    fn probe(&mut self) -> Result<(), DriverError> {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            // 16550A init: disable interrupts, set 115200 8N1, enable FIFO.
            let outb = |off: u16, val: u8| {
                core::arch::asm!(
                    "out dx, al",
                    in("dx") self.base + off,
                    in("al") val,
                    options(nomem, nostack)
                );
            };
            outb(1, 0x00); // IER: disable interrupts
            outb(3, 0x80); // LCR: DLAB on
            outb(0, 0x01); // DLL: baud lo (divisor 1 → 115200 with 1.8432 MHz clock)
            outb(1, 0x00); // DLM: baud hi
            outb(3, 0x03); // LCR: 8N1, DLAB off
            outb(2, 0xC7); // FCR: enable + clear FIFOs, 14-byte trigger
        }
        #[cfg(not(target_arch = "x86_64"))]
        unsafe { arch_serial_init(); }
        Ok(())
    }

    fn handle(&mut self, msg: ipc::Message) -> ipc::Message {
        if msg.tag == 1 {
            let len = msg.data[0] as usize;
            for &b in &msg.data[1..1 + len.min(55)] {
                self.write_byte(b);
            }
        }
        ipc::Message::empty()
    }
}
