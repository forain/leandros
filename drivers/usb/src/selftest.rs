//! Raw bulk-only-transport self-test over any [`BulkTransport`]
//! (enabled by `platform::set_selftest`, i.e. the `usb-debug` feature).
//!
//! Proves the transport end to end without the mass-storage class driver:
//! GET_MAX_LUN on the control pipe, then INQUIRY, READ CAPACITY(10) and
//! READ(10) through hand-built CBW/CSW exchanges, plus a clear-halt round
//! trip. The READs are 64 KiB (one TRB) and 192 KiB (three chained TRBs) and
//! must return identical data for the overlapping range.

use alloc::vec;
use crate::transport::{BulkTransport, UsbErr};
use crate::usb_log;

const CBW_SIG: u32 = 0x4342_5355; // "USBC"
const CSW_SIG: u32 = 0x5342_5355; // "USBS"
const TIMEOUT_MS: u32 = 5000;

/// One BOT command: CBW out, optional data in, CSW in. Returns bytes of
/// data received.
fn bot_in(t: &mut dyn BulkTransport, tag: u32, cb: &[u8], data: &mut [u8]) -> Result<usize, &'static str> {
    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
    cbw[4..8].copy_from_slice(&tag.to_le_bytes());
    cbw[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    cbw[12] = 0x80; // data IN
    cbw[13] = 0;    // LUN
    cbw[14] = cb.len() as u8;
    cbw[15..15 + cb.len()].copy_from_slice(cb);
    let n = t.bulk_out(&cbw, TIMEOUT_MS).map_err(|_| "CBW bulk_out failed")?;
    if n != 31 { return Err("CBW short write"); }
    let got = if data.is_empty() { 0 } else {
        match t.bulk_in(data, TIMEOUT_MS) {
            Ok(n) => n,
            Err(UsbErr::Stall) => { t.clear_halt_in().map_err(|_| "clear_halt_in failed")?; 0 }
            Err(_) => return Err("data bulk_in failed"),
        }
    };
    let mut csw = [0u8; 13];
    let n = match t.bulk_in(&mut csw, TIMEOUT_MS) {
        Ok(n) => n,
        Err(UsbErr::Stall) => {
            t.clear_halt_in().map_err(|_| "clear_halt_in failed")?;
            t.bulk_in(&mut csw, TIMEOUT_MS).map_err(|_| "CSW bulk_in failed after clear")?
        }
        Err(_) => return Err("CSW bulk_in failed"),
    };
    if n != 13 { return Err("CSW wrong length"); }
    if u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]) != CSW_SIG { return Err("CSW bad signature"); }
    if u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]) != tag { return Err("CSW tag mismatch"); }
    if csw[12] != 0 { return Err("CSW status not good"); }
    Ok(got)
}

fn ascii(b: &[u8]) -> alloc::string::String {
    b.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' }).collect::<alloc::string::String>()
        .trim().into()
}

/// Run the self-test; logs a single PASS/FAIL line.
pub fn run(t: &mut dyn BulkTransport, label: &str) -> bool {
    match run_inner(t) {
        Ok(()) => { usb_log!("[USB-SELFTEST] {}: PASS\n", label); true }
        Err(e) => { usb_log!("[USB-SELFTEST] {}: FAIL: {}\n", label, e); false }
    }
}

fn run_inner(t: &mut dyn BulkTransport) -> Result<(), &'static str> {
    let iface = t.interface_number() as u16;
    // GET_MAX_LUN (0xFE). A STALL means "single LUN" and is allowed.
    let mut lun = [0u8; 1];
    let max_lun = match t.class_control_in(0xFE, 0, iface, &mut lun) {
        Ok(1) => lun[0],
        Ok(_) | Err(UsbErr::Stall) => 0,
        Err(_) => return Err("GET_MAX_LUN failed"),
    };

    let mut inq = [0u8; 36];
    let n = bot_in(t, 1, &[0x12, 0, 0, 0, 36, 0], &mut inq)?;
    if n < 36 { return Err("INQUIRY short"); }
    usb_log!("[USB-SELFTEST] GET_MAX_LUN={} INQUIRY type={:02x} vendor='{}' product='{}' rev='{}'\n",
             max_lun, inq[0] & 0x1F, ascii(&inq[8..16]), ascii(&inq[16..32]), ascii(&inq[32..36]));

    // An unsolicited bulk IN (no command pending): the device must STALL or
    // NAK it. Either way the transport has to come back: STALL leaves the
    // endpoint halted (→ Reset Endpoint path), a NAK runs into the timeout
    // (→ Stop Endpoint + Set TR Dequeue path).
    let mut junk = [0u8; 13];
    let r = t.bulk_in(&mut junk, 300);
    usb_log!("[USB-SELFTEST] unsolicited bulk IN -> {:?}\n", r);
    match r {
        Err(UsbErr::Stall) | Err(UsbErr::Timeout) => {}
        _ => return Err("unsolicited bulk IN neither stalled nor timed out"),
    }
    t.clear_halt_in().map_err(|_| "clear_halt_in (halted) failed")?;
    let n = bot_in(t, 6, &[0x12, 0, 0, 0, 36, 0], &mut inq)?;
    if n < 36 { return Err("INQUIRY after recovery short"); }

    // Host-side toggle reset + CLEAR_FEATURE on both endpoints while idle,
    // then make sure the pipe still works.
    t.clear_halt_in().map_err(|_| "clear_halt_in (idle) failed")?;
    t.clear_halt_out().map_err(|_| "clear_halt_out (idle) failed")?;

    // TEST UNIT READY may report a pending UNIT ATTENTION; ignore its status.
    let _ = bot_in(t, 2, &[0; 6], &mut []);

    let mut cap = [0u8; 8];
    bot_in(t, 3, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut cap)?;
    let last_lba = u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]);
    let blksz = u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]);
    usb_log!("[USB-SELFTEST] READ CAPACITY: {} blocks of {} bytes\n", last_lba as u64 + 1, blksz);
    if blksz == 0 || blksz > 4096 { return Err("implausible block size"); }

    let small = 65536usize;
    let big = 3 * 65536usize;
    let disk = (last_lba as usize + 1) * blksz as usize;
    if disk < big { return Ok(()); }
    let mut a = vec![0u8; small];
    let mut b = vec![0xA5u8; big];
    let read10 = |n: usize| {
        let blocks = (n / blksz as usize) as u16;
        [0x28, 0, 0, 0, 0, 0, 0, (blocks >> 8) as u8, blocks as u8, 0]
    };
    if bot_in(t, 4, &read10(small), &mut a)? != small { return Err("READ(10) 64K short"); }
    if bot_in(t, 5, &read10(big), &mut b)? != big { return Err("READ(10) 192K short"); }
    if a[..] != b[..small] { return Err("READ data mismatch between 64K and 192K reads"); }
    let sum: u32 = b.iter().fold(0u32, |s, &x| s.wrapping_mul(31).wrapping_add(x as u32));
    usb_log!("[USB-SELFTEST] READ(10) 64K+192K consistent, checksum {:08x}, sector0 tail {:02x}{:02x}\n",
             sum, b[510], b[511]);
    Ok(())
}
