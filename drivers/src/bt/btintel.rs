//! Intel Bluetooth controllers with the "new" (secure) bootloader: 8260,
//! 8265, 9260-era legacy variants (hw_platform 0x37, hw_variant 0x0b/0x0c/
//! 0x11/0x12/0x13/0x14). Same flow as Linux btintel's legacy bootloader
//! setup:
//!
//! 1. Intel Read Version (0xfc05). fw_variant 0x06 = bootloader, 0x23 =
//!    operational firmware already running (warm reboot / kexec): skip to 5.
//! 2. Intel Read Boot Params (0xfc0d): dev_revid names the firmware,
//!    `intel/ibt-<hw_variant>-<dev_revid>.sfi`.
//! 3. Secure send (0xfc09, sent on the bulk OUT pipe in bootloader mode):
//!    CSS header (128 B, fragment type 0), public key (256 B, type 3),
//!    signature (256 B, type 2), then the command stream after byte 644
//!    (type 1) in chunks ending on a command boundary that are a multiple
//!    of 4 bytes, every fragment <= 252 bytes. The bootloader answers with
//!    vendor event ff/06 (secure send result).
//! 4. Intel Reset (0xfc01) with the boot address the stream's 0xfc0e
//!    command carries; vendor event ff/02 = booted into operational mode.
//! 5. DDC (0xfc8b per record of `ibt-<v>-<r>.ddc`), Intel event mask (0xfc52).
//!
//! Pure (no hardware access): the transport is the [`Link`] trait, so the
//! whole sequence is host-tested against a scripted controller.
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const OP_RESET: u16 = 0xfc01;
pub const OP_READ_VERSION: u16 = 0xfc05;
pub const OP_SECURE_SEND: u16 = 0xfc09;
pub const OP_READ_BOOT_PARAMS: u16 = 0xfc0d;
pub const OP_BOOT_PARAM: u16 = 0xfc0e;
pub const OP_WRITE_DDC: u16 = 0xfc8b;
pub const OP_EVENT_MASK: u16 = 0xfc52;

pub const FW_BOOTLOADER: u8 = 0x06;
pub const FW_OPERATIONAL: u8 = 0x23;
const RSA_HEADER: usize = 644;
const FRAG_MAX: usize = 252;

pub trait Link {
    /// Sends one HCI command (`op_lo op_hi plen params…`, no H4 type);
    /// `bulk` = on the bulk OUT pipe (secure send in bootloader mode).
    fn send_cmd(&mut self, pkt: &[u8], bulk: bool) -> bool;
    /// Next HCI event (`evt plen params…`), `None` after `timeout_ms`.
    fn event(&mut self, timeout_ms: u32) -> Option<Vec<u8>>;
    fn now_ms(&mut self) -> u64;
    /// The controller left the bootloader: bulk IN carries ACL from now on.
    fn operational(&mut self) {}
    fn log(&mut self, _msg: &str) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub hw_platform: u8,
    pub hw_variant: u8,
    pub hw_revision: u8,
    pub fw_variant: u8,
    pub fw_revision: u8,
    pub fw_build_num: u8,
    pub fw_build_ww: u8,
    pub fw_build_yy: u8,
    pub fw_patch_num: u8,
}

impl Version {
    pub fn parse(ret: &[u8]) -> Option<Version> {
        if ret.len() < 10 || ret[0] != 0 { return None; }
        Some(Version {
            hw_platform: ret[1], hw_variant: ret[2], hw_revision: ret[3], fw_variant: ret[4],
            fw_revision: ret[5], fw_build_num: ret[6], fw_build_ww: ret[7], fw_build_yy: ret[8],
            fw_patch_num: ret[9],
        })
    }
    pub fn describe(&self) -> String {
        format!("platform {:#04x} variant {:#04x} rev {:#04x} fw {} {}.{}.{}.{} patch {}",
                self.hw_platform, self.hw_variant, self.hw_revision,
                match self.fw_variant { FW_BOOTLOADER => "bootloader", FW_OPERATIONAL => "operational", _ => "?" },
                self.fw_revision >> 4, self.fw_build_num, self.fw_build_ww, 2000 + self.fw_build_yy as u32,
                self.fw_patch_num)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootParams {
    pub dev_revid: u16,
    pub secure_boot: u8,
    pub otp_bdaddr: [u8; 6],
    pub limited_cce: u8,
}

impl BootParams {
    pub fn parse(ret: &[u8]) -> Option<BootParams> {
        if ret.len() < 23 || ret[0] != 0 { return None; }
        let mut a = [0u8; 6];
        a.copy_from_slice(&ret[12..18]);
        Some(BootParams { dev_revid: u16::from_le_bytes([ret[4], ret[5]]), secure_boot: ret[6], otp_bdaddr: a, limited_cce: ret[21] })
    }
}

/// Supported legacy-bootloader hardware variants.
pub fn supported(v: &Version) -> bool {
    v.hw_platform == 0x37 && matches!(v.hw_variant, 0x0b | 0x0c | 0x11 | 0x12 | 0x13 | 0x14)
}

pub fn fw_base(v: &Version, bp: &BootParams) -> String { format!("ibt-{}-{}", v.hw_variant, bp.dev_revid) }

/// The secure-send fragments of an RSA `.sfi` image: (fragment type, bytes)
/// plus the boot address from its 0xfc0e command (0 if none).
pub fn sfi_fragments(fw: &[u8]) -> Result<(Vec<(u8, &[u8])>, u32), String> {
    if fw.len() < RSA_HEADER + 3 { return Err(format!("firmware too short ({} bytes)", fw.len())); }
    // CSS header: module type 0x00000006 (RSA); the ECDSA (v2) layout is not handled here.
    if fw[0..4] != [6, 0, 0, 0] { return Err(format!("unsupported CSS header {:02x?}", &fw[0..4])); }
    let mut v: Vec<(u8, &[u8])> = Vec::new();
    v.push((0x00, &fw[0..128]));
    v.push((0x03, &fw[128..384]));
    v.push((0x02, &fw[388..644]));
    let mut boot = 0u32;
    let (mut start, mut frag) = (RSA_HEADER, 0usize);
    while start < fw.len() {
        let c = start + frag;
        if c + 3 > fw.len() { return Err(format!("command header past the end at {}", c)); }
        let op = u16::from_le_bytes([fw[c], fw[c + 1]]);
        let plen = fw[c + 2] as usize;
        if c + 3 + plen > fw.len() { return Err(format!("command {:#06x} past the end at {}", op, c)); }
        if op == OP_BOOT_PARAM && plen >= 4 {
            boot = u32::from_le_bytes([fw[c + 3], fw[c + 4], fw[c + 5], fw[c + 6]]);
        }
        frag += 3 + plen;
        if frag % 4 == 0 {
            v.push((0x01, &fw[start..start + frag]));
            start += frag;
            frag = 0;
        }
    }
    Ok((v, boot))
}

/// The 0xfc8b parameter blocks of a `.ddc` file (each `len id_lo id_hi data…`).
pub fn ddc_records(ddc: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < ddc.len() {
        let n = ddc[i] as usize + 1;
        if n < 3 || i + n > ddc.len() { return Err(format!("bad ddc record at {}", i)); }
        v.push(&ddc[i..i + n]);
        i += n;
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Timeout(u16),
    Status(u16, u8),
    Send(u16),
    Unsupported(String),
    Firmware(String),
}

impl Error {
    pub fn describe(&self) -> String {
        match self {
            Error::Timeout(op) => format!("command {:#06x} timed out", op),
            Error::Status(op, s) => format!("command {:#06x} failed, status {:#04x}", op, s),
            Error::Send(op) => format!("command {:#06x} not sent", op),
            Error::Unsupported(m) | Error::Firmware(m) => m.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub boot: Version,
    pub oper: Version,
    pub fw: String,
    pub fw_bytes: usize,
    pub fragments: usize,
    pub boot_addr: u32,
    pub download_ms: u64,
    pub ddc_records: usize,
    pub already_operational: bool,
}

struct Ctx<'a, L: Link> {
    l: &'a mut L,
    send_result: Option<(u8, u8)>,
    booted: bool,
}

impl<'a, L: Link> Ctx<'a, L> {
    fn vendor(&mut self, p: &[u8]) {
        match p.first() {
            // Secure send result: result, opcode(2), status.
            Some(0x06) if p.len() >= 5 => self.send_result = Some((p[1], p[4])),
            Some(0x02) => self.booted = true,
            _ => {}
        }
    }

    fn wait_event(&mut self, timeout_ms: u32, mut want: impl FnMut(&mut Self, &[u8]) -> Option<Result<Vec<u8>, Error>>)
                  -> Option<Result<Vec<u8>, Error>> {
        let end = self.l.now_ms() + timeout_ms as u64;
        loop {
            let now = self.l.now_ms();
            if now >= end { return None; }
            let Some(e) = self.l.event((end - now) as u32) else { continue };
            if e.len() < 2 { continue; }
            if e[0] == 0xff { self.vendor(&e[2..]); }
            if let Some(r) = want(self, &e) { return Some(r); }
        }
    }

    /// Sends a command and returns its Command Complete return parameters
    /// (status first). A successful Command Status counts as done.
    fn cmd(&mut self, op: u16, params: &[u8], bulk: bool, timeout_ms: u32) -> Result<Vec<u8>, Error> {
        let mut pkt = Vec::with_capacity(3 + params.len());
        pkt.extend_from_slice(&op.to_le_bytes());
        pkt.push(params.len() as u8);
        pkt.extend_from_slice(params);
        if !self.l.send_cmd(&pkt, bulk) { return Err(Error::Send(op)); }
        let r = self.wait_event(timeout_ms, |_, e| match e[0] {
            0x0e if e.len() >= 5 && u16::from_le_bytes([e[3], e[4]]) == op => Some(Ok(e[5..].to_vec())),
            0x0f if e.len() >= 6 && u16::from_le_bytes([e[4], e[5]]) == op => {
                Some(if e[2] == 0 { Ok(alloc::vec![0]) } else { Err(Error::Status(op, e[2])) })
            }
            _ => None,
        });
        r.unwrap_or(Err(Error::Timeout(op)))
    }

    fn version(&mut self) -> Result<Version, Error> {
        let r = self.cmd(OP_READ_VERSION, &[], false, 2000)?;
        Version::parse(&r).ok_or(Error::Status(OP_READ_VERSION, r.first().copied().unwrap_or(0xff)))
    }

    fn secure_send(&mut self, ty: u8, mut data: &[u8]) -> Result<usize, Error> {
        let mut n = 0;
        let mut p = [0u8; FRAG_MAX + 1];
        while !data.is_empty() {
            let k = data.len().min(FRAG_MAX);
            p[0] = ty;
            p[1..1 + k].copy_from_slice(&data[..k]);
            self.cmd(OP_SECURE_SEND, &p[..1 + k], true, 5000)?;
            if let Some((res, st)) = self.send_result {
                if res != 0 || st != 0 {
                    return Err(Error::Firmware(format!("secure send rejected: result {:#04x} status {:#04x}", res, st)));
                }
            }
            data = &data[k..];
            n += 1;
        }
        Ok(n)
    }
}

/// Runs the whole setup. `load(name)` returns the file `intel/<name>`.
pub fn setup<L: Link>(l: &mut L, load: &mut dyn FnMut(&str) -> Option<Vec<u8>>) -> Result<Report, Error> {
    let mut c = Ctx { l, send_result: None, booted: false };
    let mut rep = Report::default();
    let v = c.version()?;
    c.l.log(&format!("version: {}", v.describe()));
    if !supported(&v) {
        return Err(Error::Unsupported(format!("unsupported Intel controller (platform {:#04x} variant {:#04x})",
                                              v.hw_platform, v.hw_variant)));
    }
    rep.boot = v;
    let base;
    if v.fw_variant == FW_OPERATIONAL {
        c.l.log("operational firmware already running, download skipped");
        rep.already_operational = true;
        c.l.operational();
        // Without the boot params (bootloader only) the DDC name falls back
        // to the 8265's revision 16 for variant 0x0c.
        base = format!("ibt-{}-{}", v.hw_variant, if v.hw_variant == 0x0c { 16 } else { 0 });
        rep.oper = v;
    } else if v.fw_variant != FW_BOOTLOADER {
        return Err(Error::Unsupported(format!("unknown firmware variant {:#04x}", v.fw_variant)));
    } else {
        let r = c.cmd(OP_READ_BOOT_PARAMS, &[], false, 2000)?;
        let bp = BootParams::parse(&r).ok_or(Error::Status(OP_READ_BOOT_PARAMS, r.first().copied().unwrap_or(0xff)))?;
        if bp.limited_cce != 0 {
            return Err(Error::Unsupported(format!("limited CCE device ({:#04x})", bp.limited_cce)));
        }
        let a = bp.otp_bdaddr;
        c.l.log(&format!("boot params: dev_revid {} secure_boot {} otp bdaddr {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                         bp.dev_revid, bp.secure_boot, a[5], a[4], a[3], a[2], a[1], a[0]));
        base = fw_base(&v, &bp);
        let name = format!("{}.sfi", base);
        let fw = load(&name).ok_or_else(|| Error::Firmware(format!("firmware intel/{} missing", name)))?;
        let (frags, boot) = sfi_fragments(&fw).map_err(|e| Error::Firmware(format!("{}: {}", name, e)))?;
        rep.fw = name;
        rep.fw_bytes = fw.len();
        rep.boot_addr = boot;
        let t0 = c.l.now_ms();
        for (ty, d) in &frags {
            rep.fragments += c.secure_send(*ty, d)?;
        }
        // The bootloader verifies the image, then reports (ff/06).
        if c.send_result.is_none() {
            let _ = c.wait_event(5000, |s, _| s.send_result.map(|_| Ok(Vec::new())));
        }
        match c.send_result {
            None => return Err(Error::Firmware(String::from("no secure send result after the download"))),
            Some((0, 0)) => {}
            Some((r, s)) => return Err(Error::Firmware(format!("firmware rejected: result {:#04x} status {:#04x}", r, s))),
        }
        rep.download_ms = c.l.now_ms() - t0;
        c.l.log(&format!("{}: {} bytes in {} fragments, {} ms, boot address {:#010x}",
                         rep.fw, rep.fw_bytes, rep.fragments, rep.download_ms, boot));
        // Intel Reset: reset_type 0, patch_enable 1, ddc_reload 0, boot_option 1, boot_param.
        let mut p = [0u8; 8];
        p[1] = 1;
        p[3] = 1;
        p[4..8].copy_from_slice(&boot.to_le_bytes());
        let mut pkt = Vec::with_capacity(11);
        pkt.extend_from_slice(&OP_RESET.to_le_bytes());
        pkt.push(8);
        pkt.extend_from_slice(&p);
        if !c.l.send_cmd(&pkt, false) { return Err(Error::Send(OP_RESET)); }
        if !c.booted {
            let _ = c.wait_event(5000, |s, _| if s.booted { Some(Ok(Vec::new())) } else { None });
        }
        if !c.booted { return Err(Error::Firmware(String::from("no boot-up event after Intel Reset"))); }
        c.l.operational();
        let o = c.version()?;
        c.l.log(&format!("booted: {}", o.describe()));
        if o.fw_variant != FW_OPERATIONAL {
            return Err(Error::Firmware(format!("still not operational after boot (fw variant {:#04x})", o.fw_variant)));
        }
        rep.oper = o;
    }
    let ddc_name = format!("{}.ddc", base);
    match load(&ddc_name) {
        None => c.l.log(&format!("intel/{} missing, no DDC applied", ddc_name)),
        Some(ddc) => {
            let recs = ddc_records(&ddc).map_err(|e| Error::Firmware(format!("{}: {}", ddc_name, e)))?;
            for r in &recs {
                let ret = c.cmd(OP_WRITE_DDC, r, false, 2000)?;
                if ret.first().copied().unwrap_or(0) != 0 { return Err(Error::Status(OP_WRITE_DDC, ret[0])); }
            }
            rep.ddc_records = recs.len();
            c.l.log(&format!("{}: {} records applied", ddc_name, recs.len()));
        }
    }
    if let Err(e) = c.cmd(OP_EVENT_MASK, &[0x87, 0, 0, 0, 0, 0, 0, 0], false, 2000) {
        c.l.log(&format!("event mask: {}", e.describe()));
    }
    Ok(rep)
}
