//! BCDC protocol (brcmfmac `bcdc.c`) on top of SDPCM, and the [`Fwil`]
//! implementation the cfg/netdev layer talks to.
//!
//! Control: a 16-byte `brcmf_proto_bcdc_dcmd` header + payload on the SDPCM
//! control channel; the response carries the same request id.
//! Data/events: a 4-byte BCDC header (+ `data_offset` words of firmware
//! signals) in front of an Ethernet frame on the data/event channels.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use super::bus::SdioBus;
use super::chip::Chip;
use super::fwil::{FwError, Fwil, RxSink};
use super::sdpcm::{self, Sdpcm, SdpcmError};

pub const DCMD_HDR: usize = 16;
pub const BCDC_DCMD_ERROR: u32 = 0x01;
pub const BCDC_DCMD_SET: u32 = 0x02;
pub const BCDC_DCMD_IF_SHIFT: u32 = 12;
pub const BCDC_DCMD_ID_SHIFT: u32 = 16;

pub const BCDC_HDR: usize = 4;
pub const BCDC_PROTO_VER: u8 = 2;
pub const BCDC_FLAG_VER_SHIFT: u8 = 4;

/// WLC_GET_VAR / WLC_SET_VAR.
pub const WLC_GET_VAR: u32 = 262;
pub const WLC_SET_VAR: u32 = 263;

/// Largest control message brcmfmac sends (BRCMF_TX_IOCTL_MAX_MSG_SIZE).
pub const MAX_TX_MSG: usize = 1518;
/// Control response wait (brcmfmac CTL_DONE_TIMEOUT is 2.5 s).
pub const CTL_TIMEOUT_US: u32 = 2_500_000;
/// Queued event/data frames received while waiting for a control response.
pub const MAX_PENDING: usize = 64;
pub const ETH_P_LINK_CTL: u16 = 0x886c;

impl From<SdpcmError> for FwError {
    fn from(e: SdpcmError) -> Self {
        match e {
            SdpcmError::Bus(b) => FwError::Bus(b),
            SdpcmError::TooLong => FwError::TooLong,
            SdpcmError::NoCredit => FwError::Timeout,
            _ => FwError::Bus(super::bus::BusError::Io),
        }
    }
}

/// Builds the dcmd header.
pub fn pack_dcmd(out: &mut [u8], cmd: u32, len: u32, flags: u32, status: i32) {
    out[0..4].copy_from_slice(&cmd.to_le_bytes());
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out[8..12].copy_from_slice(&flags.to_le_bytes());
    out[12..16].copy_from_slice(&status.to_le_bytes());
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BcdcStats {
    pub ctl_stale: u32,
    pub rx_dropped: u32,
    pub rx_badhdr: u32,
    pub sdpcm_errors: u32,
}

/// The SDIO Wi-Fi device: owns the bus, the chip state and the protocol state.
pub struct Brcmf<B: SdioBus> {
    pub bus: B,
    pub chip: Chip,
    pub sdpcm: Sdpcm,
    pub ifidx: u8,
    pub ctl_timeout_us: u32,
    pub stats: BcdcStats,
    reqid: u16,
    pending: VecDeque<(u8, Vec<u8>)>,
    rxbuf: Vec<u8>,
    msg: Vec<u8>,
    ctl_resp: Option<Vec<u8>>,
}

impl<B: SdioBus> Brcmf<B> {
    /// Wraps a chip whose firmware is running (after `fw::download`).
    pub fn new(bus: B, chip: Chip) -> Self {
        Brcmf {
            bus,
            chip,
            sdpcm: Sdpcm::default(),
            ifidx: 0,
            ctl_timeout_us: CTL_TIMEOUT_US,
            stats: BcdcStats::default(),
            reqid: 0,
            pending: VecDeque::new(),
            rxbuf: Vec::new(),
            msg: Vec::new(),
            ctl_resp: None,
        }
    }

    /// One service pass: interrupt status, then up to `max_frames` frames.
    /// Control frames are kept only if they answer request `want`.
    fn pump(&mut self, max_frames: usize, want: Option<u16>) -> Result<usize, FwError> {
        self.sdpcm.service_irq(&mut self.bus, &mut self.chip)?;
        let mut n = 0;
        while self.sdpcm.rx_pending && n < max_frames {
            let fr = match self.sdpcm.read_frame(&mut self.bus, &mut self.chip, &mut self.rxbuf) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(SdpcmError::Bus(b)) => return Err(FwError::Bus(b)),
                Err(_) => {
                    // Frame dropped and F2 terminated; try the next one.
                    self.stats.sdpcm_errors += 1;
                    n += 1;
                    continue;
                }
            };
            n += 1;
            let payload = &self.rxbuf[fr.doff..fr.len];
            match fr.chan {
                sdpcm::CHAN_CONTROL => {
                    let id = if payload.len() >= DCMD_HDR {
                        (u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]) >> BCDC_DCMD_ID_SHIFT) as u16
                    } else {
                        0
                    };
                    if want == Some(id) && payload.len() >= DCMD_HDR {
                        self.ctl_resp = Some(payload.to_vec());
                    } else {
                        self.stats.ctl_stale += 1;
                    }
                }
                sdpcm::CHAN_EVENT | sdpcm::CHAN_DATA => {
                    if payload.len() < BCDC_HDR || payload[0] >> BCDC_FLAG_VER_SHIFT != BCDC_PROTO_VER {
                        self.stats.rx_badhdr += 1;
                        continue;
                    }
                    let skip = BCDC_HDR + payload[3] as usize * 4;
                    if skip > payload.len() || self.pending.len() >= MAX_PENDING {
                        self.stats.rx_dropped += 1;
                        continue;
                    }
                    self.pending.push_back((fr.chan, payload[skip..].to_vec()));
                }
                _ => self.sdpcm.stats.rx_glom_dropped += 1,
            }
        }
        Ok(n)
    }

    /// Waits for the card interrupt and accounts the time against `elapsed`.
    fn wait(&mut self, elapsed: &mut u32, step: u32) {
        let got = self.bus.wait_card_irq(step);
        *elapsed = elapsed.saturating_add(if got { 50 } else { step });
    }

    fn wait_credit(&mut self, timeout_us: u32) -> Result<(), FwError> {
        let mut elapsed = 0;
        while !self.sdpcm.tx_credit() {
            if elapsed >= timeout_us {
                return Err(FwError::Timeout);
            }
            if self.pump(8, None)? == 0 {
                self.wait(&mut elapsed, 1000);
            }
        }
        Ok(())
    }

    /// brcmf_proto_bcdc_query_dcmd / set_dcmd. `data` is copied into the
    /// request, which is `reqlen` bytes long; the response payload is copied
    /// into `resp`. Returns the response payload length.
    fn dcmd(&mut self, cmd: u32, set: bool, data: &[u8], reqlen: usize, resp: &mut [u8]) -> Result<usize, FwError> {
        if data.len() > MAX_TX_MSG {
            return Err(FwError::TooLong);
        }
        let reqlen = reqlen.clamp(data.len(), MAX_TX_MSG);
        self.reqid = self.reqid.wrapping_add(1);
        let id = self.reqid;
        let flags = ((id as u32) << BCDC_DCMD_ID_SHIFT)
            | if set { BCDC_DCMD_SET } else { 0 }
            | ((self.ifidx as u32) << BCDC_DCMD_IF_SHIFT);
        let mut msg = core::mem::take(&mut self.msg);
        msg.clear();
        msg.resize(DCMD_HDR + reqlen, 0);
        pack_dcmd(&mut msg, cmd, reqlen as u32, flags, 0);
        msg[DCMD_HDR..DCMD_HDR + data.len()].copy_from_slice(data);

        self.ctl_resp = None;
        let r = self
            .wait_credit(self.ctl_timeout_us)
            .and_then(|_| Ok(self.sdpcm.send(&mut self.bus, &mut self.chip, sdpcm::CHAN_CONTROL, &msg)?));
        self.msg = msg;
        r?;

        let mut elapsed = 0u32;
        let rsp = loop {
            if let Some(r) = self.ctl_resp.take() {
                break r;
            }
            if elapsed >= self.ctl_timeout_us {
                return Err(FwError::Timeout);
            }
            if self.pump(16, Some(id))? == 0 && self.ctl_resp.is_none() {
                self.wait(&mut elapsed, 1000);
            }
        };
        let rflags = u32::from_le_bytes([rsp[8], rsp[9], rsp[10], rsp[11]]);
        if rflags & BCDC_DCMD_ERROR != 0 {
            let st = i32::from_le_bytes([rsp[12], rsp[13], rsp[14], rsp[15]]);
            return Err(FwError::Status(if st == 0 { -1 } else { st }));
        }
        let body = &rsp[DCMD_HDR..];
        let n = core::cmp::min(body.len(), resp.len());
        resp[..n].copy_from_slice(&body[..n]);
        Ok(n)
    }

    fn iovar_buf(name: &str, params: &[u8]) -> Result<Vec<u8>, FwError> {
        let mut v = Vec::with_capacity(name.len() + 1 + params.len());
        v.extend_from_slice(name.as_bytes());
        v.push(0);
        v.extend_from_slice(params);
        if v.len() > MAX_TX_MSG {
            return Err(FwError::TooLong);
        }
        Ok(v)
    }

    /// Number of received event/data frames waiting for `poll`.
    pub fn pending_frames(&self) -> usize {
        self.pending.len()
    }
}

impl<B: SdioBus> Fwil for Brcmf<B> {
    fn cmd_get(&mut self, cmd: u32, buf: &mut [u8]) -> Result<usize, FwError> {
        let req = buf.to_vec();
        self.dcmd(cmd, false, &req, req.len(), buf)
    }

    fn cmd_set(&mut self, cmd: u32, buf: &[u8]) -> Result<(), FwError> {
        self.dcmd(cmd, true, buf, buf.len(), &mut []).map(|_| ())
    }

    fn iovar_get(&mut self, name: &str, buf: &mut [u8]) -> Result<usize, FwError> {
        let req = Self::iovar_buf(name, buf)?;
        let reqlen = core::cmp::max(req.len(), buf.len());
        self.dcmd(WLC_GET_VAR, false, &req, reqlen, buf)
    }

    fn iovar_set(&mut self, name: &str, buf: &[u8]) -> Result<(), FwError> {
        let req = Self::iovar_buf(name, buf)?;
        self.dcmd(WLC_SET_VAR, true, &req, req.len(), &mut []).map(|_| ())
    }

    fn tx_data(&mut self, frame: &[u8]) -> Result<(), FwError> {
        if frame.len() > sdpcm::MAX_RX_DATASZ {
            return Err(FwError::TooLong);
        }
        self.wait_credit(10_000)?;
        let mut msg = core::mem::take(&mut self.msg);
        msg.clear();
        msg.extend_from_slice(&[BCDC_PROTO_VER << BCDC_FLAG_VER_SHIFT, 0, self.ifidx, 0]);
        msg.extend_from_slice(frame);
        let r = self.sdpcm.send(&mut self.bus, &mut self.chip, sdpcm::CHAN_DATA, &msg);
        self.msg = msg;
        Ok(r?)
    }

    fn poll(&mut self, sink: &mut dyn RxSink, budget_us: u32) -> Result<(), FwError> {
        let mut elapsed = 0u32;
        loop {
            let n = self.pump(32, None)?;
            while let Some((_, f)) = self.pending.pop_front() {
                if f.len() >= 14 && u16::from_be_bytes([f[12], f[13]]) == ETH_P_LINK_CTL {
                    sink.on_event(&f);
                } else {
                    sink.on_data(&f);
                }
            }
            if n == 0 || elapsed >= budget_us {
                return Ok(());
            }
            elapsed = elapsed.saturating_add(100);
        }
    }
}
