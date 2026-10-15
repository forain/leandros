//! Reader for the persistent console log partition (kernel/src/klog.rs), the
//! on-device twin of scripts/read-usb-log.sh: 4 slots, one per boot, each a
//! LEANLOG1 header page plus text, the newest LEANLOGJ journal page of the
//! same boot id winning over the header.
//!
//! On a running system the current boot's slot is complete up to the last
//! flush; `sync` flushes it first.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

const SLOTS: u64 = 4;
const PAGE: u64 = 4096;
const MAX_SLOT: u64 = 16 << 20;

fn crc32(data: &[u8]) -> u32 {
    static mut TABLE: [u32; 256] = [0; 256];
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| unsafe {
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            TABLE[i as usize] = c;
        }
    });
    let t = unsafe { &*core::ptr::addr_of!(TABLE) };
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn rd(f: &mut File, off: u64, n: usize) -> io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut v = vec![0u8; n];
    let mut done = 0;
    while done < n {
        let r = f.read(&mut v[done..])?;
        if r == 0 {
            v.truncate(done);
            break;
        }
        done += r;
    }
    Ok(v)
}

pub struct Boot {
    pub boot: u32,
    pub slot: u64,
    pub seq: u32,
    pub len: u64,
    pub flags: u32,
    pub journal: bool,
    pub crc_ok: bool,
    pub text: Vec<u8>,
}

impl Boot {
    pub fn notes(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.flags & 1 != 0 {
            v.push("FULL (slot filled, later output not stored)");
        }
        if self.flags & 2 != 0 {
            v.push("PANIC (kernel panicked; this is its last flush)");
        }
        if self.flags & 4 != 0 {
            v.push("LOST (output overran the RAM ring before a flush)");
        }
        if !self.crc_ok {
            v.push("CRC MISMATCH (a flush was interrupted or is in progress; the last lines may be damaged)");
        }
        v
    }
}

/// Every boot in the log partition `dev` (`size` bytes), oldest first.
pub fn read_all(dev: &str, size: u64) -> io::Result<Vec<Boot>> {
    let mut f = File::open(dev)?;
    let slot_size = (size / SLOTS).min(MAX_SLOT) & !(PAGE - 1);
    let mut out = Vec::new();
    for s in 0..SLOTS {
        let base = s * slot_size;
        let h = rd(&mut f, base, PAGE as usize)?;
        if h.len() < 56 || &h[0..8] != b"LEANLOG1" || crc32(&h[..40]) != u32le(&h, 40) {
            continue;
        }
        let boot = u32le(&h, 8);
        let mut seq = u32le(&h, 12);
        let mut len = u64le(&h, 16);
        let ssz = u64le(&h, 24);
        let mut dcrc = u32le(&h, 32);
        let mut flags = u32le(&h, 36);
        let hid = h[48..56].to_vec();
        let mut journal = false;
        let area = rd(&mut f, base + PAGE, (slot_size - PAGE) as usize)?;
        let mut i = 0usize;
        while i + 56 <= area.len() {
            let j = &area[i..i + 56];
            if &j[0..8] == b"LEANLOGJ" && crc32(&j[..40]) == u32le(j, 40) {
                let (jb, js, jl, jss, jc, jf) = (u32le(j, 8), u32le(j, 12), u64le(j, 16), u64le(j, 24), u32le(j, 32), u32le(j, 36));
                if j[48..56] == hid[..] && jb == boot && jss == ssz && (jl + PAGE - 1) / PAGE * PAGE == i as u64
                    && js.wrapping_sub(seq) < 0x8000_0000 && js != seq
                {
                    seq = js;
                    len = jl;
                    dcrc = jc;
                    flags = jf;
                    journal = true;
                }
            }
            i += PAGE as usize;
        }
        let n = len.min(slot_size - PAGE) as usize;
        let data = &area[..n.min(area.len())];
        let crc_ok = crc32(data) == dcrc;
        let text: Vec<u8> = data.iter().copied().filter(|&b| b != 0).collect();
        out.push(Boot { boot, slot: s, seq, len, flags, journal, crc_ok, text });
    }
    out.sort_by_key(|b| b.boot);
    Ok(out)
}
