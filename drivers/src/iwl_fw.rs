//! Intel Wi-Fi (iwlwifi / MVM) firmware file parser: the TLV `.ucode` format
//! of linux-firmware's `iwlwifi-*.ucode`. Hardware independent and host
//! tested (drivers/iwlwifi-tests); the kernel driver is drivers/src/iwlwifi.rs.
//!
//! File layout (all little endian): a 88-byte header (`0`, magic
//! `0x0a4c5749`, 64-byte human readable version, `ver`, `build`, 8 ignored
//! bytes) followed by TLVs `{type u32, length u32, data[length]}`, each padded
//! to a multiple of 4. The images the driver loads come from the
//! `SEC_RT`/`SEC_INIT` TLVs (and their `SECURE_*` twins): each is one section
//! `{device address u32, bytes}`, in load order. Two magic addresses separate
//! the CPU1 sections from the CPU2 ones (`0xFFFFCCCC`) and the CPU2 resident
//! sections from the paged image (`0xAAAABBBB`, followed by a CSS block and
//! the paging data, which the driver keeps in host memory and announces with
//! FW_PAGING_BLOCK_CMD after ALIVE).
//!
//! Nothing is copied: sections are (start, len) ranges into the file buffer.

use alloc::string::String;
use alloc::vec::Vec;

pub const MAGIC: u32 = 0x0a4c_5749;
pub const HEADER_LEN: usize = 88;

pub const CPU1_CPU2_SEPARATOR: u32 = 0xFFFF_CCCC;
pub const PAGING_SEPARATOR: u32 = 0xAAAA_BBBB;
/// Firmware paging granularity (4 KiB pages, grouped 8 to a 32 KiB block).
pub const FW_PAGING_SIZE: usize = 4096;
pub const PAGES_PER_GROUP: usize = 8;
pub const PAGING_BLOCK_SIZE: usize = FW_PAGING_SIZE * PAGES_PER_GROUP;
/// 32 data blocks + the CSS block.
pub const MAX_PAGING_BLOCKS: usize = 33;

// TLV types used here (fw/file.h).
pub const TLV_PROBE_MAX_LEN: u32 = 6;
pub const TLV_FLAGS: u32 = 18;
pub const TLV_SEC_RT: u32 = 19;
pub const TLV_SEC_INIT: u32 = 20;
pub const TLV_SEC_WOWLAN: u32 = 21;
pub const TLV_DEF_CALIB: u32 = 22;
pub const TLV_PHY_SKU: u32 = 23;
pub const TLV_SECURE_SEC_RT: u32 = 24;
pub const TLV_SECURE_SEC_INIT: u32 = 25;
pub const TLV_SECURE_SEC_WOWLAN: u32 = 26;
pub const TLV_NUM_OF_CPU: u32 = 27;
pub const TLV_API_CHANGES_SET: u32 = 29;
pub const TLV_ENABLED_CAPABILITIES: u32 = 30;
pub const TLV_N_SCAN_CHANNELS: u32 = 31;
pub const TLV_PAGING: u32 = 32;
pub const TLV_FW_VERSION: u32 = 36;
pub const TLV_CMD_VERSIONS: u32 = 48;
pub const TLV_FW_NUM_STATIONS: u32 = 0x100;

/// Image indices (enum iwl_ucode_type).
pub const IMG_REGULAR: usize = 0;
pub const IMG_INIT: usize = 1;
pub const IMG_WOWLAN: usize = 2;

// Capability / API bits the driver consults (fw/file.h).
pub mod capa {
    pub const LAR_SUPPORT: u32 = 1;
    pub const UMAC_SCAN: u32 = 2;
    pub const DQA_SUPPORT: u32 = 12;
    pub const LAR_MULTI_MCC: u32 = 29;
    pub const BINDING_CDB_SUPPORT: u32 = 39;
    pub const TLC_OFFLOAD: u32 = 43;
    pub const DYNAMIC_QUOTA: u32 = 44;
    pub const ULTRA_HB_CHANNELS: u32 = 48;
    pub const SESSION_PROT_CMD: u32 = 54;
    pub const BT_MPLUT_SUPPORT: u32 = 67;
    pub const LAR_SUPPORT_V3: u32 = 73;
    pub const CT_KILL_BY_FW: u32 = 74;
    pub const TEMP_THS_REPORT_SUPPORT: u32 = 75;
    pub const EXTEND_SHARED_MEM_CFG: u32 = 80;
    pub const TX_POWER_ACK: u32 = 84;
    pub const MCC_UPDATE_11AX_SUPPORT: u32 = 89;
}
pub mod api {
    pub const FRAGMENTED_SCAN: u32 = 8;
    pub const WIFI_MCC_UPDATE: u32 = 9;
    pub const LQ_SS_PARAMS: u32 = 18;
    pub const NEW_VERSION: u32 = 20;
    pub const SCAN_TSF_REPORT: u32 = 28;
    pub const TKIP_MIC_KEYS: u32 = 29;
    pub const STA_TYPE: u32 = 30;
    pub const ADAPTIVE_DWELL: u32 = 32;
    pub const NEW_BEACON_TEMPLATE: u32 = 34;
    pub const NEW_RX_STATS: u32 = 35;
    pub const QUOTA_LOW_LATENCY: u32 = 38;
    pub const ADAPTIVE_DWELL_V2: u32 = 42;
    pub const FRAG_EBS: u32 = 44;
    pub const REDUCE_TX_POWER: u32 = 45;
    pub const SCAN_EXT_CHAN_VER: u32 = 58;
}

/// One section: device address and the byte range of its data in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub offset: u32,
    pub start: usize,
    pub len: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Image {
    pub sections: Vec<Section>,
}

/// How an image is loaded on a dual-CPU (8000-family) part: the section
/// indices for CPU1 and CPU2, and the paged part of CPU2, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadPlan {
    pub cpu1: Vec<usize>,
    pub cpu2: Vec<usize>,
    /// (CSS section index, paging data section index).
    pub paging: Option<(usize, usize)>,
}

impl Image {
    /// Split the section list like iwl_pcie_load_cpu_sections_8000: CPU1
    /// from the start up to the first separator, CPU2 from after it up to
    /// the next separator (or the end); after a PAGING separator, the CSS
    /// block and the paging data.
    pub fn load_plan(&self) -> LoadPlan {
        let s = &self.sections;
        let is_sep = |x: &Section| x.offset == CPU1_CPU2_SEPARATOR || x.offset == PAGING_SEPARATOR || x.len == 0;
        let mut i = 0;
        let mut cpu1 = Vec::new();
        while i < s.len() && !is_sep(&s[i]) { cpu1.push(i); i += 1; }
        let mut cpu2 = Vec::new();
        if i < s.len() && s[i].offset == CPU1_CPU2_SEPARATOR {
            i += 1;
            while i < s.len() && !is_sep(&s[i]) { cpu2.push(i); i += 1; }
        }
        // The CSS block and the paging data follow the PAGING separator
        // (fw/paging.c: at least two sections must be left after it).
        let paging = s.iter().position(|x| x.offset == PAGING_SEPARATOR)
            .filter(|&p| p + 2 < s.len())
            .map(|p| (p + 1, p + 2));
        LoadPlan { cpu1, cpu2, paging }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FwInfo {
    /// Human readable version string from the header.
    pub human: String,
    pub ver: u32,
    pub build: u32,
    /// FW_VERSION TLV (major, minor, local_comp), if present.
    pub fw_version: Option<(u32, u32, u32)>,
    pub images: [Image; 3],
    pub flags: u32,
    pub api: [u32; 4],
    pub capa: [u32; 4],
    pub phy_config: u32,
    pub valid_tx_ant: u8,
    pub valid_rx_ant: u8,
    /// Default calibration (flow_trigger, event_trigger) per image.
    pub default_calib: [(u32, u32); 3],
    pub n_scan_channels: u32,
    pub max_probe_len: u32,
    pub num_cpus: u32,
    pub paging_mem_size: u32,
    pub num_stations: u32,
    /// (cmd, group, cmd_ver, notif_ver).
    pub cmd_versions: Vec<[u8; 4]>,
    /// TLV types seen (for the log), in file order, de-duplicated.
    pub tlv_types: Vec<u32>,
}

impl FwInfo {
    /// The API number (the `-36` of the file name): newer files carry the
    /// major version in `ver` directly, older ones in bits 8..16.
    pub fn api_major(&self) -> u32 { if self.ver < 0x100 { self.ver } else { (self.ver >> 8) & 0xff } }
    pub fn has_api(&self, bit: u32) -> bool {
        let i = (bit / 32) as usize;
        i < 4 && self.api[i] & (1 << (bit % 32)) != 0
    }
    pub fn has_capa(&self, bit: u32) -> bool {
        let i = (bit / 32) as usize;
        i < 4 && self.capa[i] & (1 << (bit % 32)) != 0
    }
    /// Command version from the CMD_VERSIONS TLV (None when not listed).
    pub fn cmd_ver(&self, group: u8, cmd: u8) -> Option<u8> {
        self.cmd_versions.iter().find(|v| v[0] == cmd && v[1] == group).map(|v| v[2])
    }
    pub fn notif_ver(&self, group: u8, cmd: u8) -> Option<u8> {
        self.cmd_versions.iter().find(|v| v[0] == cmd && v[1] == group).map(|v| v[3])
    }
}

fn le32(d: &[u8], o: usize) -> u32 { u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) }

/// Parse a TLV firmware file. Errors are short static descriptions.
pub fn parse(d: &[u8]) -> Result<FwInfo, &'static str> {
    if d.len() < HEADER_LEN { return Err("file shorter than the TLV header"); }
    if le32(d, 0) != 0 { return Err("not a TLV firmware (first word not zero)"); }
    if le32(d, 4) != MAGIC { return Err("bad TLV magic"); }
    let mut fw = FwInfo::default();
    let hr = &d[8..72];
    let n = hr.iter().position(|&b| b == 0).unwrap_or(64);
    fw.human = hr[..n].iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '?' }).collect();
    fw.ver = le32(d, 72);
    fw.build = le32(d, 76);
    fw.num_cpus = 1;
    let mut off = HEADER_LEN;
    while off + 8 <= d.len() {
        let t = le32(d, off);
        let l = le32(d, off + 4) as usize;
        let data_off = off + 8;
        if l > d.len() - data_off { return Err("TLV length runs past the end of the file"); }
        let p = &d[data_off..data_off + l];
        if !fw.tlv_types.contains(&t) { fw.tlv_types.push(t); }
        let u32_of = |p: &[u8]| -> Result<u32, &'static str> {
            if p.len() != 4 { Err("TLV with a bad u32 length") } else { Ok(le32(p, 0)) }
        };
        match t {
            TLV_SEC_RT | TLV_SEC_INIT | TLV_SEC_WOWLAN | TLV_SECURE_SEC_RT | TLV_SECURE_SEC_INIT
            | TLV_SECURE_SEC_WOWLAN => {
                if l < 4 { return Err("section TLV shorter than its address"); }
                let img = match t {
                    TLV_SEC_RT | TLV_SECURE_SEC_RT => IMG_REGULAR,
                    TLV_SEC_INIT | TLV_SECURE_SEC_INIT => IMG_INIT,
                    _ => IMG_WOWLAN,
                };
                fw.images[img].sections.push(Section { offset: le32(p, 0), start: data_off + 4, len: l - 4 });
            }
            TLV_PROBE_MAX_LEN => fw.max_probe_len = u32_of(p)?,
            TLV_FLAGS => {
                if l < 4 || l % 4 != 0 { return Err("bad FLAGS TLV"); }
                fw.flags = le32(p, 0);
            }
            TLV_API_CHANGES_SET | TLV_ENABLED_CAPABILITIES => {
                if l != 8 { return Err("bad API/CAPA TLV length"); }
                let idx = le32(p, 0) as usize;
                let v = le32(p, 4);
                if idx < 4 {
                    if t == TLV_API_CHANGES_SET { fw.api[idx] = v } else { fw.capa[idx] = v }
                }
            }
            TLV_DEF_CALIB => {
                if l != 12 { return Err("bad DEF_CALIB TLV length"); }
                let ty = le32(p, 0) as usize;
                if ty < 3 { fw.default_calib[ty] = (le32(p, 4), le32(p, 8)); }
            }
            TLV_PHY_SKU => {
                fw.phy_config = u32_of(p)?;
                fw.valid_tx_ant = ((fw.phy_config >> 16) & 0xf) as u8;
                fw.valid_rx_ant = ((fw.phy_config >> 20) & 0xf) as u8;
            }
            TLV_NUM_OF_CPU => {
                let n = u32_of(p)?;
                if !(1..=2).contains(&n) { return Err("unsupported number of CPUs"); }
                fw.num_cpus = n;
            }
            TLV_N_SCAN_CHANNELS => fw.n_scan_channels = u32_of(p)?,
            TLV_PAGING => {
                let n = u32_of(p)?;
                if n as usize > 32 * PAGING_BLOCK_SIZE { return Err("paging image too large"); }
                if n as usize % FW_PAGING_SIZE != 0 { return Err("paging image not a multiple of 4 KiB"); }
                fw.paging_mem_size = n;
            }
            TLV_FW_VERSION => {
                if l == 12 { fw.fw_version = Some((le32(p, 0), le32(p, 4), le32(p, 8))); }
            }
            TLV_CMD_VERSIONS => {
                for c in p.chunks_exact(4) { fw.cmd_versions.push([c[0], c[1], c[2], c[3]]); }
            }
            TLV_FW_NUM_STATIONS => fw.num_stations = u32_of(p)?,
            _ => {}
        }
        off = data_off + ((l + 3) & !3);
    }
    if fw.images[IMG_REGULAR].sections.is_empty() { return Err("no runtime (SEC_RT) image"); }
    Ok(fw)
}

/// The paging layout for `paging_mem_size` bytes: (number of 32 KiB data
/// blocks, pages in the last block).
pub fn paging_layout(paging_mem_size: usize) -> (usize, usize) {
    let pages = paging_mem_size / FW_PAGING_SIZE;
    if pages == 0 { return (0, 0); }
    let blocks = (pages + PAGES_PER_GROUP - 1) / PAGES_PER_GROUP;
    (blocks, pages - PAGES_PER_GROUP * (blocks - 1))
}
