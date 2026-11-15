//! Privsep IPC between the root monitor and the unprivileged network child
//! (design-sshdprivsep.md section 4).
//!
//! Frame: `u32 len (BE, <= MAX_FRAME) | u8 type | u32 req_id | body`, where `len`
//! counts everything after itself. Bodies are SSH-style fields (`u32`-prefixed
//! strings, `u8`/`u32` scalars). Two channels use it: `sign` (synchronous) and
//! `ctl` (tagged, async). Both are AF_UNIX SOCK_SEQPACKET, so one frame is one
//! datagram; the monitor reads with `recvmsg` and treats any attached SCM_RIGHTS
//! it did not ask for as fatal.
//!
//! This file has no tokio or russh dependency: it is compiled by the host unit
//! crate (userland/sshd/unit) as well.

pub const MAX_FRAME: usize = 64 * 1024;
const HDR: usize = 1 + 4; // type + req_id (after the length word)

// sign channel
pub const SIGN: u8 = 1; // child -> mon: key blob, hash alg, H
pub const SIG: u8 = 2; // mon -> child: signature blob
pub const ERR: u8 = 3; // either way: text

// ctl channel, child -> monitor
pub const AUTH_PASSWORD: u8 = 16;
pub const AUTH_PUBKEY_QUERY: u8 = 17;
pub const AUTH_PUBKEY: u8 = 18;
pub const SESSION_OPEN: u8 = 19;
pub const SESSION_SIGNAL: u8 = 20;
pub const SESSION_HANGUP: u8 = 21;
pub const DIRECT_TCPIP: u8 = 22;
pub const TCPIP_FORWARD: u8 = 23;
// ctl channel, monitor -> child
pub const AUTH_OK: u8 = 32;
pub const AUTH_FAIL: u8 = 33;
pub const SESSION_FDS: u8 = 34;
pub const FWD_FD: u8 = 35;
pub const FWD_LISTEN: u8 = 36;
pub const SESSION_EXIT: u8 = 37; // req_id 0: monitor event
pub const SHUTDOWN: u8 = 38; // req_id 0: monitor event
pub const OK: u8 = 39;

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub ty: u8,
    pub req: u32,
    pub body: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RpcError {
    Short,
    TooLong,
    Trailing,
    Truncated,
}

pub fn encode(ty: u8, req: u32, body: &[u8]) -> Result<Vec<u8>, RpcError> {
    let len = HDR + body.len();
    if len > MAX_FRAME {
        return Err(RpcError::TooLong);
    }
    let mut v = Vec::with_capacity(4 + len);
    v.extend((len as u32).to_be_bytes());
    v.push(ty);
    v.extend(req.to_be_bytes());
    v.extend(body);
    Ok(v)
}

/// Parse exactly one frame from a whole datagram. Anything left over, or a length
/// word that disagrees with the datagram size, is an error (fatal for the peer).
pub fn decode(dg: &[u8]) -> Result<Frame, RpcError> {
    if dg.len() < 4 + HDR {
        return Err(RpcError::Short);
    }
    let len = u32::from_be_bytes([dg[0], dg[1], dg[2], dg[3]]) as usize;
    if len > MAX_FRAME {
        return Err(RpcError::TooLong);
    }
    if len < HDR || 4 + len > dg.len() {
        return Err(RpcError::Truncated);
    }
    if 4 + len < dg.len() {
        return Err(RpcError::Trailing);
    }
    Ok(Frame { ty: dg[4], req: u32::from_be_bytes([dg[5], dg[6], dg[7], dg[8]]), body: dg[9..].to_vec() })
}

#[derive(Default)]
pub struct W(pub Vec<u8>);
impl W {
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend(v.to_be_bytes());
        self
    }
    pub fn str(&mut self, s: &[u8]) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend(s);
        self
    }
}

pub struct R<'a>(pub &'a [u8]);
impl<'a> R<'a> {
    pub fn u8(&mut self) -> Result<u8, RpcError> {
        let (&b, rest) = self.0.split_first().ok_or(RpcError::Truncated)?;
        self.0 = rest;
        Ok(b)
    }
    pub fn u32(&mut self) -> Result<u32, RpcError> {
        let b = self.0.get(..4).ok_or(RpcError::Truncated)?;
        let v = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        self.0 = &self.0[4..];
        Ok(v)
    }
    pub fn str(&mut self) -> Result<&'a [u8], RpcError> {
        let n = self.u32()? as usize;
        let s = self.0.get(..n).ok_or(RpcError::Truncated)?;
        self.0 = &self.0[n..];
        Ok(s)
    }
    /// Fails unless every byte was consumed.
    pub fn done(&self) -> Result<(), RpcError> {
        if self.0.is_empty() { Ok(()) } else { Err(RpcError::Trailing) }
    }
}

/// Send one frame (and optionally fds via SCM_RIGHTS) on a SEQPACKET socket.
#[cfg(target_os = "linux")]
pub fn send(sock: i32, ty: u8, req: u32, body: &[u8], fds: &[i32]) -> std::io::Result<()> {
    let buf = encode(ty, req, body).map_err(|_| std::io::Error::from_raw_os_error(libc::EMSGSIZE))?;
    let mut iov = libc::iovec { iov_base: buf.as_ptr() as *mut _, iov_len: buf.len() };
    let mut cbuf = [0u64; 8]; // aligned room for up to 4 fds
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let n = fds.len() * 4;
        assert!(fds.len() <= 4);
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = unsafe { libc::CMSG_SPACE(n as u32) } as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(n as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(c), n);
        }
    }
    loop {
        let r = unsafe { libc::sendmsg(sock, &msg, 0) };
        if r >= 0 {
            return if r as usize == buf.len() { Ok(()) } else { Err(std::io::Error::from_raw_os_error(libc::EIO)) };
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Receive one datagram; returns the frame and any fds that came with it
/// (CLOEXEC set). `Ok(None)` on orderly EOF; WouldBlock on an empty nonblocking
/// socket. The caller decides whether the fds were expected.
#[cfg(target_os = "linux")]
pub fn recv(sock: i32) -> std::io::Result<Option<(Result<Frame, RpcError>, Vec<i32>)>> {
    let mut buf = vec![0u8; 4 + MAX_FRAME + 1];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    let n = loop {
        let r = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if r >= 0 {
            break r as usize;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let cnt = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                let p = libc::CMSG_DATA(c) as *const i32;
                for i in 0..cnt {
                    fds.push(std::ptr::read_unaligned(p.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if n == 0 && fds.is_empty() {
        return Ok(None);
    }
    Ok(Some((decode(&buf[..n]), fds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut w = W::default();
        w.u8(7).u32(0xdead).str(b"hello");
        let f = decode(&encode(AUTH_PASSWORD, 5, &w.0).unwrap()).unwrap();
        assert_eq!((f.ty, f.req), (AUTH_PASSWORD, 5));
        let mut r = R(&f.body);
        assert_eq!((r.u8().unwrap(), r.u32().unwrap(), r.str().unwrap()), (7, 0xdead, &b"hello"[..]));
        r.done().unwrap();
    }

    #[test]
    fn rejects_malformed() {
        let ok = encode(SIGN, 1, b"abc").unwrap();
        assert_eq!(decode(&ok[..8]), Err(RpcError::Short));
        assert_eq!(decode(&ok[..ok.len() - 1]), Err(RpcError::Truncated));
        let mut extra = ok.clone();
        extra.push(0);
        assert_eq!(decode(&extra), Err(RpcError::Trailing));
        let mut big = ok.clone();
        big[..4].copy_from_slice(&((MAX_FRAME as u32) + 1).to_be_bytes());
        assert_eq!(decode(&big), Err(RpcError::TooLong));
        assert_eq!(encode(SIGN, 0, &vec![0; MAX_FRAME]), Err(RpcError::TooLong));
        let mut small = ok.clone();
        small[..4].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(decode(&small), Err(RpcError::Truncated));
    }

    #[test]
    fn reader_bounds() {
        let mut w = W::default();
        w.u32(1000); // string length far beyond the body
        assert_eq!(R(&w.0).str(), Err(RpcError::Truncated));
        assert_eq!(R(&[1, 2]).u32(), Err(RpcError::Truncated));
        assert_eq!(R(&[1]).done(), Err(RpcError::Trailing));
        assert_eq!(R(&[]).u8(), Err(RpcError::Truncated));
    }

    #[test]
    fn fuzz_decode_never_panics() {
        let mut x = 0x1234_5678u32;
        for _ in 0..20000 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = (x >> 24) as usize % 40;
            let v: Vec<u8> = (0..n).map(|i| (x >> (i % 24)) as u8).collect();
            let _ = decode(&v);
            let mut r = R(&v);
            let _ = (r.str(), r.u32());
        }
    }
}
