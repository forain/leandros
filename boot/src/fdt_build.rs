//! Fresh flattened-device-tree writer (kexec, aarch64).
//!
//! Produces the DTB handed to `kernel-direct` in x0. Pure and
//! allocation-free: the blob is written into a caller-provided buffer (which
//! must be 8-byte aligned wherever it finally lives; the arm64 boot protocol
//! wants that). Only what `boot::device_tree::{parse, for_each_cpu_mpidr}`
//! reads is emitted, plus a psci node for Linux-correctness:
//!
//! ```text
//! / { #address-cells=<2> #size-cells=<2> compatible model
//!   chosen { bootargs linux,initrd-start/end (u64) rng-seed stdout-path }
//!   memory@<first> { device_type="memory" reg=<base len>... }   (<= 60)
//!   pl011@<uart>   { compatible reg=<uart 0x1000> }
//!   pcie@<ecam>    { compatible="pci-host-ecam-generic" reg=<ecam size> }
//!   [framebuffer@<addr> { reg width height stride format }]
//!   cpus { #address-cells #size-cells=0  cpu@<mpidr> { device_type reg enable-method } }
//!   psci { compatible method }
//! }
//! ```
//! No `/reserved-memory` and no `/memreserve/` (the parser reads neither): the
//! caller omits anything unusable from `memory`.

use crate::memmap::{MemoryRegion, MemoryType};

/// `boot::device_tree::parse` keeps at most this many `memory` reg entries.
pub const FDT_MAX_MEMORY: usize = 60;
/// The direct aarch64 early tables map 0-16 GiB; usable RAM above is dropped.
pub const FDT_MAP_LIMIT: u64 = 16 << 30;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;
const HDR: usize = 40;
const STRINGS_CAP: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdtError {
    BufferTooSmall,
    TooManyMemory,
    TooManyStrings,
}

#[derive(Clone, Copy, Debug)]
pub struct FdtFb {
    pub addr: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

pub struct FdtInput<'a> {
    /// Kernel command line without NUL (empty = no bootargs).
    pub bootargs: &'a [u8],
    /// Initrd `(start, length)`.
    pub initrd: Option<(u64, u64)>,
    /// Up to 64 bytes for `/chosen/rng-seed` (empty = none).
    pub rng_seed: &'a [u8],
    /// Usable RAM `(base, length)`, at most [`FDT_MAX_MEMORY`] entries.
    pub memory: &'a [(u64, u64)],
    /// PL011 base, 0 = no node.
    pub uart_base: u64,
    /// PCIe ECAM base, 0 = no node; `ecam_size` is its `reg` length.
    pub pci_ecam_base: u64,
    pub ecam_size: u64,
    pub framebuffer: Option<FdtFb>,
    /// MPIDR affinity of each CPU, boot CPU first.
    pub cpus: &'a [u64],
    /// PSCI conduit, `b"hvc"` or `b"smc"` (empty = no psci node).
    pub psci_method: &'a [u8],
}

/// Pick up to [`FDT_MAX_MEMORY`] ranges from a map: Available only, clipped
/// to `[0, FDT_MAP_LIMIT)`, sorted as given; when over the cap the smallest
/// are dropped. Returns the count written to `out`.
pub fn select_memory(regions: &[MemoryRegion], out: &mut [(u64, u64); FDT_MAX_MEMORY]) -> usize {
    let mut n = 0usize;
    for r in regions {
        if r.kind != MemoryType::Available || r.base >= FDT_MAP_LIMIT { continue; }
        let end = r.end().min(FDT_MAP_LIMIT);
        if end <= r.base { continue; }
        if n == FDT_MAX_MEMORY {
            // Replace the smallest if this one is larger.
            let mut s = 0;
            for i in 1..n { if out[i].1 < out[s].1 { s = i; } }
            if out[s].1 >= end - r.base { continue; }
            for i in s..n - 1 { out[i] = out[i + 1]; }
            n -= 1;
        }
        out[n] = (r.base, end - r.base);
        n += 1;
    }
    n
}

struct Fdt<'a> {
    buf: &'a mut [u8],
    off: usize, // struct block write cursor (absolute)
    strings: [u8; STRINGS_CAP],
    slen: usize,
    err: Option<FdtError>,
}

impl<'a> Fdt<'a> {
    fn fail(&mut self, e: FdtError) { if self.err.is_none() { self.err = Some(e); } }
    fn raw(&mut self, b: &[u8]) {
        if self.off + b.len() > self.buf.len() { self.fail(FdtError::BufferTooSmall); return; }
        self.buf[self.off..self.off + b.len()].copy_from_slice(b);
        self.off += b.len();
    }
    fn pad4(&mut self) { while self.off % 4 != 0 && self.err.is_none() { self.raw(&[0]); } }
    fn tok(&mut self, t: u32) { self.raw(&t.to_be_bytes()); }
    fn begin(&mut self, name: &[u8]) {
        self.tok(FDT_BEGIN_NODE);
        self.raw(name);
        self.raw(&[0]);
        self.pad4();
    }
    fn end(&mut self) { self.tok(FDT_END_NODE); }
    fn name_off(&mut self, name: &[u8]) -> u32 {
        // Linear search for an existing NUL-terminated copy.
        let mut i = 0;
        while i < self.slen {
            let mut j = i;
            while self.strings[j] != 0 { j += 1; }
            if &self.strings[i..j] == name { return i as u32; }
            i = j + 1;
        }
        if self.slen + name.len() + 1 > STRINGS_CAP { self.fail(FdtError::TooManyStrings); return 0; }
        let at = self.slen;
        self.strings[at..at + name.len()].copy_from_slice(name);
        self.strings[at + name.len()] = 0;
        self.slen += name.len() + 1;
        at as u32
    }
    fn prop(&mut self, name: &[u8], data: &[u8]) {
        let no = self.name_off(name);
        self.tok(FDT_PROP);
        self.raw(&(data.len() as u32).to_be_bytes());
        self.raw(&no.to_be_bytes());
        self.raw(data);
        self.pad4();
    }
    fn p_u32(&mut self, name: &[u8], v: u32) { self.prop(name, &v.to_be_bytes()); }
    fn p_u64(&mut self, name: &[u8], v: u64) { self.prop(name, &v.to_be_bytes()); }
    /// NUL-terminated string (or several, already NUL-separated by caller).
    fn p_str(&mut self, name: &[u8], s: &[u8]) {
        // Build in a small stack buffer to append the terminator.
        let mut tmp = [0u8; 96];
        if s.len() + 1 > tmp.len() { self.fail(FdtError::BufferTooSmall); return; }
        tmp[..s.len()].copy_from_slice(s);
        self.prop(name, &tmp[..s.len() + 1]);
    }
}

/// `prefix` + lowercase hex of `v` -> node name in `out`; returns the length.
fn node_name(out: &mut [u8; 48], prefix: &[u8], v: u64) -> usize {
    out[..prefix.len()].copy_from_slice(prefix);
    let mut n = prefix.len();
    let mut started = false;
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xf) as u8;
        if d != 0 || started || i == 0 {
            started = true;
            out[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            n += 1;
        }
    }
    n
}

/// Build the DTB into `buf`; returns its total size.
pub fn build_fdt(input: &FdtInput, buf: &mut [u8]) -> Result<usize, FdtError> {
    if input.memory.len() > FDT_MAX_MEMORY { return Err(FdtError::TooManyMemory); }
    if buf.len() < HDR + 16 { return Err(FdtError::BufferTooSmall); }
    for b in buf[..HDR + 16].iter_mut() { *b = 0; }
    let mut f = Fdt { buf, off: HDR + 16, strings: [0; STRINGS_CAP], slen: 0, err: None };
    let mut nm = [0u8; 48];

    f.begin(b"");
    f.p_u32(b"#address-cells", 2);
    f.p_u32(b"#size-cells", 2);
    f.p_str(b"compatible", b"linux,dummy-virt");
    f.p_str(b"model", b"LeandrOS kexec");

    // /chosen
    f.begin(b"chosen");
    if !input.bootargs.is_empty() {
        // bootargs can be long: write directly with its terminator.
        let no = f.name_off(b"bootargs");
        f.tok(FDT_PROP);
        f.raw(&((input.bootargs.len() + 1) as u32).to_be_bytes());
        f.raw(&no.to_be_bytes());
        f.raw(input.bootargs);
        f.raw(&[0]);
        f.pad4();
    }
    if let Some((s, l)) = input.initrd {
        f.p_u64(b"linux,initrd-start", s);
        f.p_u64(b"linux,initrd-end", s + l);
    }
    if !input.rng_seed.is_empty() {
        let n = input.rng_seed.len().min(64);
        f.prop(b"rng-seed", &input.rng_seed[..n]);
    }
    if input.uart_base != 0 {
        let n = node_name(&mut nm, b"/pl011@", input.uart_base);
        let mut tmp = [0u8; 48];
        tmp[..n].copy_from_slice(&nm[..n]);
        f.p_str(b"stdout-path", &tmp[..n]);
    }
    f.end();

    // /memory
    if !input.memory.is_empty() {
        let n = node_name(&mut nm, b"memory@", input.memory[0].0);
        f.begin(&nm[..n]);
        f.p_str(b"device_type", b"memory");
        let no = f.name_off(b"reg");
        f.tok(FDT_PROP);
        f.raw(&((input.memory.len() * 16) as u32).to_be_bytes());
        f.raw(&no.to_be_bytes());
        for &(b, l) in input.memory {
            f.raw(&b.to_be_bytes());
            f.raw(&l.to_be_bytes());
        }
        f.pad4();
        f.end();
    }

    if input.uart_base != 0 {
        let n = node_name(&mut nm, b"pl011@", input.uart_base);
        f.begin(&nm[..n]);
        f.prop(b"compatible", b"arm,pl011\0arm,primecell\0");
        let mut reg = [0u8; 16];
        reg[..8].copy_from_slice(&input.uart_base.to_be_bytes());
        reg[8..].copy_from_slice(&0x1000u64.to_be_bytes());
        f.prop(b"reg", &reg);
        f.p_str(b"status", b"okay");
        f.end();
    }

    if input.pci_ecam_base != 0 {
        let n = node_name(&mut nm, b"pcie@", input.pci_ecam_base);
        f.begin(&nm[..n]);
        f.p_str(b"device_type", b"pci");
        f.p_str(b"compatible", b"pci-host-ecam-generic");
        f.p_u32(b"#address-cells", 3);
        f.p_u32(b"#size-cells", 2);
        f.prop(b"bus-range", &[0, 0, 0, 0, 0, 0, 0, 0xff]);
        let mut reg = [0u8; 16];
        reg[..8].copy_from_slice(&input.pci_ecam_base.to_be_bytes());
        let sz = if input.ecam_size != 0 { input.ecam_size } else { 0x1000_0000 };
        reg[8..].copy_from_slice(&sz.to_be_bytes());
        f.prop(b"reg", &reg);
        f.end();
    }

    if let Some(fb) = input.framebuffer {
        let n = node_name(&mut nm, b"framebuffer@", fb.addr);
        f.begin(&nm[..n]);
        f.p_str(b"compatible", b"simple-framebuffer");
        let mut reg = [0u8; 16];
        reg[..8].copy_from_slice(&fb.addr.to_be_bytes());
        reg[8..].copy_from_slice(&((fb.stride as u64) * (fb.height as u64)).to_be_bytes());
        f.prop(b"reg", &reg);
        f.p_u32(b"width", fb.width);
        f.p_u32(b"height", fb.height);
        f.p_u32(b"stride", fb.stride);
        f.p_str(b"format", b"a8r8g8b8");
        f.end();
    }

    if !input.cpus.is_empty() {
        let wide = input.cpus.iter().any(|&m| m > u32::MAX as u64);
        f.begin(b"cpus");
        f.p_u32(b"#address-cells", if wide { 2 } else { 1 });
        f.p_u32(b"#size-cells", 0);
        for &m in input.cpus {
            let n = node_name(&mut nm, b"cpu@", m);
            f.begin(&nm[..n]);
            f.p_str(b"device_type", b"cpu");
            f.p_str(b"compatible", b"arm,armv8");
            if wide { f.p_u64(b"reg", m); } else { f.p_u32(b"reg", m as u32); }
            if !input.psci_method.is_empty() { f.p_str(b"enable-method", b"psci"); }
            f.end();
        }
        f.end();
    }

    if !input.psci_method.is_empty() {
        f.begin(b"psci");
        f.prop(b"compatible", b"arm,psci-1.0\0arm,psci-0.2\0");
        let m = input.psci_method;
        f.p_str(b"method", m);
        f.end();
    }

    f.end(); // root
    f.tok(FDT_END);

    if let Some(e) = f.err { return Err(e); }
    let struct_end = f.off;
    let size_struct = struct_end - (HDR + 16);
    let strings_off = struct_end;
    let total = (strings_off + f.slen + 7) & !7;
    if total > f.buf.len() { return Err(FdtError::BufferTooSmall); }
    let slen = f.slen;
    let strings = f.strings;
    f.buf[strings_off..strings_off + slen].copy_from_slice(&strings[..slen]);
    for b in f.buf[strings_off + slen..total].iter_mut() { *b = 0; }
    let hdr: [u32; 10] = [
        0xD00D_FEED, total as u32, (HDR + 16) as u32, strings_off as u32, HDR as u32,
        17, 16, 0, slen as u32, size_struct as u32,
    ];
    for (i, v) in hdr.iter().enumerate() {
        f.buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    Ok(total)
}
