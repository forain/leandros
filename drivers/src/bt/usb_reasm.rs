//! HCI packet reassembly for the USB transport: the endpoints deliver raw
//! USB packets (an event split over several interrupt packets, an ACL frame
//! over several bulk packets); this rebuilds whole H4 packets from the
//! packet's own length field. Pure, host-tested (drivers/bt-tests).
use alloc::vec::Vec;

pub const H4_ACL: u8 = 0x02;
pub const H4_EVENT: u8 = 0x04;
/// Largest ACL payload accepted (btusb buffers are HCI_MAX_FRAME_SIZE).
const ACL_MAX: usize = 4096;

pub struct Reasm {
    ty: u8,
    buf: Vec<u8>,
    /// Bytes thrown away because a length field was impossible.
    pub resyncs: u64,
}

impl Reasm {
    pub const fn new(ty: u8) -> Self { Reasm { ty, buf: Vec::new(), resyncs: 0 } }

    pub fn reset(&mut self) { self.buf.clear(); }

    pub fn pending(&self) -> usize { self.buf.len() }

    /// Appends one USB chunk; every completed packet is pushed to `out`
    /// with its H4 type byte first.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        self.buf.extend_from_slice(chunk);
        loop {
            let (hdr, len) = match self.ty {
                H4_EVENT => {
                    if self.buf.len() < 2 { return; }
                    (2usize, self.buf[1] as usize)
                }
                _ => {
                    if self.buf.len() < 4 { return; }
                    (4usize, u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize)
                }
            };
            if self.ty == H4_ACL && len > ACL_MAX {
                self.resyncs += self.buf.len() as u64;
                self.buf.clear();
                return;
            }
            let total = hdr + len;
            if self.buf.len() < total { return; }
            let mut p = Vec::with_capacity(total + 1);
            p.push(self.ty);
            p.extend_from_slice(&self.buf[..total]);
            out.push(p);
            self.buf.drain(..total);
        }
    }
}
