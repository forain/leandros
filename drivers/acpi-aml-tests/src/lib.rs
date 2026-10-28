//! Host tests for the AML interpreter additions of lane s3aml: OpRegion
//! accesses above the EC space reach the host, Field(Lock) takes the global
//! lock around each access, and `_S3_`/`_PTS`/`_WAK` evaluate.

#[path = "../../src/acpi_power/aml.rs"]
pub mod aml;
#[path = "../../src/acpi_power/gl.rs"]
pub mod gl;

#[cfg(test)]
mod tests {
    use super::aml::*;

    #[derive(Default)]
    struct Fake {
        reads: Vec<(u8, u64, u8)>,
        writes: Vec<(u8, u64, u8, u64)>,
        acq: u32,
        rel: u32,
        cmos: u64,
        gl_fail: bool,
    }
    impl Host for Fake {
        fn read(&mut self, s: u8, a: u64, w: u8) -> u64 { self.reads.push((s, a, w)); self.cmos }
        fn write(&mut self, s: u8, a: u64, w: u8, v: u64) { self.writes.push((s, a, w, v)); }
        fn sleep_ms(&mut self, _: u64) {}
        fn stall_us(&mut self, _: u64) {}
        fn timer(&mut self) -> u64 { 0 }
        fn notify(&mut self, _: usize, _: u64) {}
        fn log(&mut self, _: &str) {}
        fn global_lock(&mut self, a: bool) -> bool { if a { if self.gl_fail { return false; } self.acq += 1 } else { self.rel += 1 } true }
    }

    fn dsdt() -> &'static [u8] {
        let mut body: Vec<u8> = vec![];
        // OperationRegion(CMS_, SystemCMOS, 0x10, 4); Field(CMS_, ByteAcc, Lock) { BYT0, 8 }
        body.extend([0x5B, 0x80, b'C', b'M', b'S', b'_', 0x05, 0x0A, 0x10, 0x0A, 0x04]);
        body.extend([0x5B, 0x81, 0x0B, b'C', b'M', b'S', b'_', 0x11, b'B', b'Y', b'T', b'0', 0x08]);
        // OperationRegion(GPO_, GPIO, 0, 4); Field(GPO_, ByteAcc, NoLock) { GP00, 8 }
        body.extend([0x5B, 0x80, b'G', b'P', b'O', b'_', 0x08, 0x00, 0x0A, 0x04]);
        body.extend([0x5B, 0x81, 0x0B, b'G', b'P', b'O', b'_', 0x01, b'G', b'P', b'0', b'0', 0x08]);
        // Method(_PTS,1) { BYT0 = Arg0 }
        body.extend([0x14, 0x0C, b'_', b'P', b'T', b'S', 0x01, 0x70, 0x68, b'B', b'Y', b'T', b'0']);
        // Method(_TTS,1) { GP00 = Arg0 }
        body.extend([0x14, 0x0C, b'_', b'T', b'T', b'S', 0x01, 0x70, 0x68, b'G', b'P', b'0', b'0']);
        // Method(_WAK,1) { Return (Package(2){0,0}) }
        body.extend([0x14, 0x0C, b'_', b'W', b'A', b'K', 0x01, 0xA4, 0x12, 0x04, 0x02, 0x00, 0x00]);
        // Name(_S3_, Package(2){5,1})
        body.extend([0x08, b'_', b'S', b'3', b'_', 0x12, 0x05, 0x02, 0x0A, 0x05, 0x01]);
        // Method(RDC_,0) { Return (BYT0) }
        body.extend([0x14, 0x0B, b'R', b'D', b'C', b'_', 0x00, 0xA4, b'B', b'Y', b'T', b'0']);
        let mut t = vec![0u8; 36];
        t[..4].copy_from_slice(b"DSDT");
        t.extend(body);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        t[8] = 2;
        Box::leak(t.into_boxed_slice())
    }

    fn load() -> (Ns, Fake) {
        let mut ns = Ns::new();
        let mut h = Fake::default();
        ns.load_table(&mut h, dsdt());
        (ns, h)
    }

    #[test]
    fn s3_package_evaluates() {
        let (mut ns, mut h) = load();
        let n = ns.find("\\_S3_").expect("_S3_");
        let v = ns.eval(&mut h, n, &[]).unwrap();
        let e = v.elements().unwrap();
        assert_eq!((ns.int_of(&mut h, &e[0]), ns.int_of(&mut h, &e[1])), (Some(5), Some(1)));
    }

    #[test]
    fn pts_writes_cmos_field_under_global_lock() {
        let (mut ns, mut h) = load();
        let n = ns.find("\\_PTS").unwrap();
        ns.eval(&mut h, n, &[V::Int(3)]).unwrap();
        assert_eq!(h.writes.iter().filter(|w| w.0 == SPACE_CMOS && w.1 == 0x10 && w.3 == 3).count(), 1, "{:?}", h.writes);
        assert_eq!((h.acq, h.rel), (1, 1));
    }

    #[test]
    fn cmos_read_reaches_host_and_is_locked() {
        let (mut ns, mut h) = load();
        h.cmos = 0x5A;
        let n = ns.find("\\RDC_").unwrap();
        let v = ns.eval(&mut h, n, &[]).unwrap();
        let v = ns.resolve(&mut h, &v);
        assert_eq!(ns.int_of(&mut h, &v), Some(0x5A));
        assert!(h.reads.iter().any(|r| r.0 == SPACE_CMOS && r.1 == 0x10));
        assert_eq!((h.acq, h.rel), (1, 1));
    }

    #[test]
    fn gpio_write_reaches_host_without_lock() {
        let (mut ns, mut h) = load();
        let n = ns.find("\\_TTS").unwrap();
        ns.eval(&mut h, n, &[V::Int(3)]).unwrap();
        assert!(h.writes.iter().any(|w| w.0 == 8 && w.3 == 3), "{:?}", h.writes);
        assert_eq!((h.acq, h.rel), (0, 0));
    }

    #[test]
    fn wak_returns_status_pss_package() {
        let (mut ns, mut h) = load();
        let n = ns.find("\\_WAK").unwrap();
        let v = ns.eval(&mut h, n, &[V::Int(3)]).unwrap();
        assert_eq!(v.elements().unwrap().len(), 2);
    }

    #[test]
    fn lock_timeout_fails_field_access_without_touching_host() {
        let (mut ns, mut h) = load();
        h.gl_fail = true;
        let n = ns.find("\\_PTS").unwrap();
        assert!(ns.eval(&mut h, n, &[V::Int(3)]).is_err());
        assert!(h.writes.is_empty(), "{:?}", h.writes);
        assert_eq!((h.acq, h.rel), (0, 0));
    }

    #[test]
    fn gl_acquire_release_clean() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let g = crate::gl::Gl::new();
        let w = AtomicU32::new(0);
        assert!(g.acquire(&w, 10, &mut || {}));
        assert_eq!(w.load(Ordering::Relaxed), 2);
        assert!(g.acquire(&w, 10, &mut || {})); // nested
        assert!(!g.release(&w));
        assert_eq!(w.load(Ordering::Relaxed), 2); // still held
        assert!(!g.release(&w));
        assert_eq!(w.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn gl_timeout_withdraws_pending_and_keeps_firmware_ownership() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let g = crate::gl::Gl::new();
        let w = AtomicU32::new(2); // firmware owns, never releases
        let mut spins = 0;
        assert!(!g.acquire(&w, 50, &mut || spins += 1));
        assert_eq!(spins, 50);
        assert_eq!(w.load(Ordering::Relaxed), 2, "firmware owned bit kept, our pending cleared");
        assert!(!g.owned());
        assert!(!g.release(&w)); // must not touch FACS
        assert_eq!(w.load(Ordering::Relaxed), 2);
        assert_eq!(g.fails.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn gl_acquires_after_firmware_releases_and_reports_pending() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let g = crate::gl::Gl::new();
        let w = AtomicU32::new(2);
        let mut n = 0;
        // firmware releases on the 3rd delay (clears owned+pending)
        assert!(g.acquire(&w, 100, &mut || { n += 1; if n == 3 { w.store(0, Ordering::Relaxed); } }));
        assert!(g.owned());
        assert_eq!(w.load(Ordering::Relaxed), 2);
        // firmware sets pending while we hold it -> release must report GBL_RLS needed
        w.fetch_or(1, Ordering::Relaxed);
        assert!(g.release(&w));
        assert_eq!(w.load(Ordering::Relaxed), 0);
    }
}
