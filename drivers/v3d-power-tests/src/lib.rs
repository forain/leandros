//! Host tests for the V3D power-up. `regs.rs` and `power.rs` are included
//! verbatim, so these exercise exactly the code the kernel runs, against a
//! scripted register file (`MockRegs`) and firmware/PM environment (`MockEnv`).

#[path = "../../src/v3d/regs.rs"]
pub mod regs;
#[path = "../../src/v3d/power.rs"]
pub mod power;

#[cfg(test)]
mod tests {
    use super::power::*;
    use super::regs::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq)]
    enum Ev {
        PmR(u32),
        PmW(u32, u32),
        ClkState(u32, bool),
        ClkMax(u32),
        ClkRate(u32, u32),
        HubR(u32),
        HubW(u32, u32),
        CoreR(u32),
        CoreW(u32, u32),
        SmsR(u32),
        SmsW(u32, u32),
    }

    type Log = Rc<RefCell<Vec<Ev>>>;

    /// A BCM2712 V3D after power-up: what the registers read.
    struct MockRegs {
        log: Log,
        hub: RefCell<HashMap<u32, u32>>,
        core: RefCell<HashMap<u32, u32>>,
        /// SMS state reported on reads: TEE reads stay `tee_busy` for this
        /// many reads after the CLEAR_POWER_OFF write, then go idle.
        tee_busy_reads: Cell<u32>,
        ree_busy_reads: Cell<u32>,
        tee_stuck: bool,
        ree_stuck: bool,
    }

    impl MockRegs {
        fn pi5(log: Log) -> Self {
            let mut hub = HashMap::new();
            hub.insert(HUB_IDENT0, 0x4248_5556);
            hub.insert(HUB_IDENT1, 0x0002_0117);
            hub.insert(HUB_IDENT2, 0x0000_0000);
            hub.insert(HUB_IDENT3, 0x0004_0600);
            hub.insert(MMU_DEBUG_INFO, 0x0000_0620); // pa 30+6=36, va 30+2=32
            let mut core = HashMap::new();
            core.insert(CTL_IDENT0, 0x0744_3356);
            core.insert(CTL_IDENT1, 0x2000_0421);
            MockRegs { log, hub: RefCell::new(hub), core: RefCell::new(core), tee_busy_reads: Cell::new(3), ree_busy_reads: Cell::new(2), tee_stuck: false, ree_stuck: false }
        }
    }

    impl V3dRegs for MockRegs {
        fn hub_r(&self, off: u32) -> u32 { self.log.borrow_mut().push(Ev::HubR(off)); *self.hub.borrow().get(&off).unwrap_or(&0) }
        fn hub_w(&self, off: u32, v: u32) { self.log.borrow_mut().push(Ev::HubW(off, v)); self.hub.borrow_mut().insert(off, v); }
        fn core_r(&self, off: u32) -> u32 { self.log.borrow_mut().push(Ev::CoreR(off)); *self.core.borrow().get(&off).unwrap_or(&0) }
        fn core_w(&self, off: u32, v: u32) { self.log.borrow_mut().push(Ev::CoreW(off, v)); self.core.borrow_mut().insert(off, v); }
        fn sms_r(&self, off: u32) -> u32 {
            self.log.borrow_mut().push(Ev::SmsR(off));
            if off == SMS_TEE_CS {
                if self.tee_stuck { return SMS_STATE_POWER_OFF; }
                let n = self.tee_busy_reads.get();
                if n > 0 { self.tee_busy_reads.set(n - 1); return SMS_STATE_POWER_OFF; }
                SMS_STATE_IDLE
            } else {
                if self.ree_stuck { return SMS_STATE_RESETTING; }
                let n = self.ree_busy_reads.get();
                if n > 0 { self.ree_busy_reads.set(n - 1); return if n % 2 == 0 { SMS_STATE_ISOLATING_FOR_RESET } else { SMS_STATE_RESETTING }; }
                SMS_STATE_IDLE
            }
        }
        fn sms_w(&self, off: u32, v: u32) { self.log.borrow_mut().push(Ev::SmsW(off, v)); }
    }

    struct MockEnv {
        log: Log,
        pm: u32,
        /// PM_GRAFX (the GRAFX island). Starts off, as the Pi 5 firmware leaves it.
        grafx: u32,
        /// Lowest inrush step at which POWOK comes up (None = never).
        grafx_powok_at: Option<u32>,
        /// MEMREP completes (MRDONE).
        grafx_mrdone: bool,
        /// PM_GRAFX_2712 ignores writes (models a wrong password or a locked PM).
        pm_ignores: bool,
        clk_state_reply: Option<u32>,
        clk_max: Option<u32>,
        clk_rate_ok: bool,
        now: u64,
        /// Microseconds each now_us() call advances (0 = frozen clock).
        tick: u64,
        delays: u64,
    }

    impl MockEnv {
        fn pi5(log: Log) -> Self {
            MockEnv { log, pm: 0x0000_1000, grafx: 0x0000_1000, grafx_powok_at: Some(1), grafx_mrdone: true, pm_ignores: false, clk_state_reply: Some(CLK_STATE_ON), clk_max: Some(960_000_000), clk_rate_ok: true, now: 0, tick: 1, delays: 0 }
        }
    }

    impl PowerEnv for MockEnv {
        fn pm_r(&mut self, off: u32) -> u32 {
            self.log.borrow_mut().push(Ev::PmR(off));
            if off == PM_GRAFX { self.grafx } else { self.pm }
        }
        fn pm_w(&mut self, off: u32, v: u32) {
            self.log.borrow_mut().push(Ev::PmW(off, v));
            if v & 0xff00_0000 != PM_PASSWORD { return; }
            let v = v & 0x00ff_ffff;
            if off == PM_GRAFX {
                let inrush = (v & PM_INRUSH_MASK) >> PM_INRUSH_SHIFT;
                let mut g = v & !(PM_POWOK | PM_MRDONE);
                if v & PM_POWUP != 0 && self.grafx_powok_at.map_or(false, |l| inrush >= l) { g |= PM_POWOK; }
                if v & PM_MEMREP != 0 && self.grafx_mrdone { g |= PM_MRDONE; }
                self.grafx = g;
            } else if !self.pm_ignores {
                self.pm = v;
            }
        }
        fn clk_set_state(&mut self, id: u32, on: bool) -> Option<u32> { self.log.borrow_mut().push(Ev::ClkState(id, on)); self.clk_state_reply }
        fn clk_get_max_rate(&mut self, id: u32) -> Option<u32> { self.log.borrow_mut().push(Ev::ClkMax(id)); self.clk_max }
        fn clk_set_rate(&mut self, id: u32, hz: u32) -> Option<u32> { self.log.borrow_mut().push(Ev::ClkRate(id, hz)); if self.clk_rate_ok { Some(hz) } else { None } }
        fn delay_us(&mut self, us: u32) { self.delays += us as u64; self.now += us as u64; }
        fn now_us(&mut self) -> u64 { self.now += self.tick; self.now }
    }

    fn setup() -> (Log, MockRegs, MockEnv) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        (log.clone(), MockRegs::pi5(log.clone()), MockEnv::pi5(log))
    }

    fn pos(log: &[Ev], f: impl Fn(&Ev) -> bool) -> usize {
        log.iter().position(|e| f(e)).expect("event not found")
    }

    fn is_v3d(e: &Ev) -> bool {
        matches!(e, Ev::HubR(_) | Ev::HubW(..) | Ev::CoreR(_) | Ev::CoreW(..) | Ev::SmsR(_) | Ev::SmsW(..))
    }

    #[test]
    fn pi5_powers_up_and_identifies() {
        let (_log, regs, mut env) = setup();
        let (dev, rep) = power_on(regs, &mut env).expect("power_on");
        assert_eq!((dev.ver, dev.va_width), (71, 32));
        assert_eq!((rep.ver, rep.rev, rep.cores, rep.qpus, rep.va_width, rep.pa_width), (71, 6, 1, 8, 32, 36));
        assert_eq!(rep.clk_hz, 960_000_000);
        assert_eq!(rep.sms_state, SMS_STATE_IDLE);
        assert_eq!((rep.pm_before, rep.pm_after), (0x1000, 0x1000 | PM_V3DRSTN));
    }

    #[test]
    fn sequence_order_pm_clock_sms_then_ident() {
        let (log, regs, mut env) = setup();
        power_on(regs, &mut env).expect("power_on");
        let l = log.borrow();
        let pm_w = pos(&l, |e| matches!(e, Ev::PmW(PM_GRAFX_2712, _)));
        let clk_on = pos(&l, |e| matches!(e, Ev::ClkState(..)));
        let clk_max = pos(&l, |e| matches!(e, Ev::ClkMax(_)));
        let clk_rate = pos(&l, |e| matches!(e, Ev::ClkRate(..)));
        let first_v3d = pos(&l, is_v3d);
        let tee = pos(&l, |e| *e == Ev::SmsW(SMS_TEE_CS, SMS_CLEAR_POWER_OFF));
        let ree = pos(&l, |e| *e == Ev::SmsW(SMS_REE_CS, SMS_STATE_RESET_REQ));
        let ident0 = pos(&l, |e| *e == Ev::CoreR(CTL_IDENT0));
        let flsta = pos(&l, |e| *e == Ev::CoreW(CTL_L2TFLSTA, 0));
        let flend = pos(&l, |e| *e == Ev::CoreW(CTL_L2TFLEND, !0));
        let hid1 = pos(&l, |e| *e == Ev::HubR(HUB_IDENT1));
        assert!(pm_w < clk_on && clk_on < clk_max && clk_max < clk_rate);
        assert!(clk_rate < first_v3d, "no V3D register access before PM + clock");
        assert_eq!(first_v3d, tee, "the first V3D access is the SMS power-on");
        assert!(tee < ree && ree < ident0 && ident0 < flsta && flsta < flend && flend < hid1);
        // Clock set to exactly the max rate, on clock id 5.
        assert!(l.contains(&Ev::ClkState(5, true)));
        assert!(l.contains(&Ev::ClkRate(5, 960_000_000)));
    }

    #[test]
    fn pm_write_carries_password_and_preserves_bits() {
        let (log, regs, mut env) = setup();
        env.pm = 0x0000_1003;
        power_on(regs, &mut env).expect("power_on");
        let l = log.borrow();
        let w: Vec<_> = l.iter().filter_map(|e| if let Ev::PmW(o, v) = e { if *o == PM_GRAFX_2712 { Some((*o, *v)) } else { None } } else { None }).collect();
        assert_eq!(w, vec![(PM_GRAFX_2712, 0x5a00_1043)]);
        assert!(env.delays >= 2, "1 us settle on either side of the deassert");
    }

    #[test]
    fn pm_password_never_doubled_from_readback() {
        // A readback with junk in [31:24] must not corrupt the password byte.
        let (log, regs, mut env) = setup();
        env.pm = 0x7f00_0001;
        let _ = power_on(regs, &mut env);
        let l = log.borrow();
        let v = l.iter().find_map(|e| if let Ev::PmW(PM_GRAFX_2712, v) = e { Some(*v) } else { None }).unwrap();
        assert_eq!(v, 0x5a00_0041);
    }

    #[test]
    fn pm_not_taken_is_inert() {
        let (log, regs, mut env) = setup();
        env.pm_ignores = true;
        let (e, rep) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e, PowerError::Mismatch { step: Step::Pm, value: 0x1000 });
        assert_eq!(rep.pm_after, 0x1000);
        let l = log.borrow();
        assert!(!l.iter().any(is_v3d), "no V3D access after a PM failure");
        assert!(!l.iter().any(|e| matches!(e, Ev::ClkState(..))));
    }

    #[test]
    fn clock_failures_are_inert() {
        for case in 0..4 {
            let (log, regs, mut env) = setup();
            match case {
                0 => env.clk_state_reply = None,
                1 => env.clk_state_reply = Some(CLK_STATE_NOT_EXIST),
                2 => env.clk_max = Some(0),
                _ => env.clk_rate_ok = false,
            }
            let (e, _) = power_on(regs, &mut env).err().expect("must fail");
            let want = if case < 2 { Step::ClkState } else { Step::ClkRate };
            assert_eq!(e.step(), want, "case {case}");
            assert!(matches!(e, PowerError::Firmware { .. }));
            assert!(!log.borrow().iter().any(is_v3d), "case {case}: no V3D access without a clock");
        }
    }

    #[test]
    fn sms_power_timeout_is_bounded_and_inert() {
        let (log, mut regs, mut env) = setup();
        regs.tee_stuck = true;
        let (e, _) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e, PowerError::Timeout { step: Step::SmsPowerOn, last: SMS_STATE_POWER_OFF });
        assert!(env.now >= POLL_TIMEOUT_US && env.now < POLL_TIMEOUT_US * 3, "bounded at ~100 ms, got {} us", env.now);
        let l = log.borrow();
        assert!(!l.contains(&Ev::SmsW(SMS_REE_CS, SMS_STATE_RESET_REQ)), "no reset after a failed power-on");
        assert!(!l.iter().any(|e| matches!(e, Ev::HubR(_) | Ev::CoreR(_) | Ev::CoreW(..))));
    }

    #[test]
    fn sms_reset_timeout_is_bounded_and_inert() {
        let (log, mut regs, mut env) = setup();
        regs.ree_stuck = true;
        let (e, _) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e, PowerError::Timeout { step: Step::SmsReset, last: SMS_STATE_RESETTING });
        assert!(!log.borrow().iter().any(|e| matches!(e, Ev::HubR(_) | Ev::CoreR(_))));
    }

    #[test]
    fn frozen_clock_still_terminates() {
        let (_log, mut regs, mut env) = setup();
        regs.tee_stuck = true;
        env.tick = 0;
        env.grafx_powok_at = Some(0);
        // delay_us still advances `now` in the mock; freeze it completely.
        struct Frozen(MockEnv);
        impl PowerEnv for Frozen {
            fn pm_r(&mut self, o: u32) -> u32 { self.0.pm_r(o) }
            fn pm_w(&mut self, o: u32, v: u32) { self.0.pm_w(o, v) }
            fn clk_set_state(&mut self, i: u32, on: bool) -> Option<u32> { self.0.clk_set_state(i, on) }
            fn clk_get_max_rate(&mut self, i: u32) -> Option<u32> { self.0.clk_get_max_rate(i) }
            fn clk_set_rate(&mut self, i: u32, h: u32) -> Option<u32> { self.0.clk_set_rate(i, h) }
            fn delay_us(&mut self, us: u32) { self.0.delays += us as u64; }
            fn now_us(&mut self) -> u64 { 0 }
        }
        let mut f = Frozen(env);
        let (e, _) = power_on(regs, &mut f).err().expect("must fail");
        assert_eq!(e.step(), Step::SmsPowerOn);
        assert_eq!(f.0.delays, 2 + POLL_MAX_ITERS as u64, "iteration cap ends the poll");
    }

    #[test]
    fn sms_reset_waits_through_isolating_and_resetting() {
        let (log, regs, mut env) = setup();
        power_on(regs, &mut env).expect("power_on");
        let ree_reads = log.borrow().iter().filter(|e| **e == Ev::SmsR(SMS_REE_CS)).count();
        assert_eq!(ree_reads, 3, "two busy reads then idle");
    }

    #[test]
    fn unclocked_block_fails_ident() {
        let (_log, regs, mut env) = setup();
        regs.core.borrow_mut().insert(CTL_IDENT0, 0);
        let (e, _) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e, PowerError::Mismatch { step: Step::Ident, value: 0 });
    }

    #[test]
    fn wrong_version_or_cores_rejected() {
        let (_l, regs, mut env) = setup();
        regs.hub.borrow_mut().insert(HUB_IDENT1, 0x0002_0124); // 4.2
        assert_eq!(power_on(regs, &mut env).err().unwrap().0.step(), Step::Version);
        let (_l, regs, mut env) = setup();
        regs.hub.borrow_mut().insert(HUB_IDENT1, 0x0002_0217); // 2 cores
        assert_eq!(power_on(regs, &mut env).err().unwrap().0.step(), Step::Cores);
    }

    #[test]
    fn hub_ident1_encoding() {
        // The value the DRM stub answers for HUB_IDENT1 must decode as a
        // single-core V3D 7.1 with a TFU, under Linux's field layout.
        let v = 0x0002_0117u32;
        assert_eq!(hub_ident1_tver(v) * 10 + hub_ident1_rev(v), 71);
        assert_eq!(hub_ident1_ncores(v), 1);
        assert_ne!(v & HUB_IDENT1_WITH_TFU, 0);
        // The old stub value decoded as "V3D 1.0".
        let old = 0x0700_0101u32;
        assert_eq!(hub_ident1_tver(old) * 10 + hub_ident1_rev(old), 10);
        let src = include_str!("../../src/drm_device_interface.rs");
        assert!(src.contains("const V3D_HUB_IDENT1: u64 = 0x0002_0117;"), "stub must carry the fixed encoding");
    }

    fn grafx_writes(l: &[Ev]) -> Vec<u32> {
        l.iter().filter_map(|e| if let Ev::PmW(PM_GRAFX, v) = e { Some(*v) } else { None }).collect()
    }

    #[test]
    fn grafx_island_on_before_v3drstn_clock_and_any_v3d_access() {
        let (log, regs, mut env) = setup();
        let (_, rep) = power_on(regs, &mut env).expect("power_on");
        let l = log.borrow();
        // Inrush ramp: 3.5 mA (no POWOK) then the next step; stop at the first POWOK.
        let w = grafx_writes(&l);
        let p = PM_PASSWORD | 0x1000;
        assert_eq!(w, vec![
            p | PM_POWUP,
            p | (1 << PM_INRUSH_SHIFT) | PM_POWUP,
            p | (1 << PM_INRUSH_SHIFT) | PM_POWUP | PM_POWOK | PM_ISPOW,
            p | (1 << PM_INRUSH_SHIFT) | PM_POWUP | PM_POWOK | PM_ISPOW | PM_MEMREP,
            p | (1 << PM_INRUSH_SHIFT) | PM_POWUP | PM_POWOK | PM_ISPOW | PM_MEMREP | PM_MRDONE | PM_ISFUNC,
        ]);
        let isfunc = pos(&l, |e| matches!(e, Ev::PmW(PM_GRAFX, v) if v & PM_ISFUNC != 0));
        let rstn = pos(&l, |e| matches!(e, Ev::PmW(PM_GRAFX_2712, _)));
        let clk = pos(&l, |e| matches!(e, Ev::ClkRate(..)));
        let first_v3d = pos(&l, is_v3d);
        // The confirm re-read of both PM registers sits between the clock and the first V3D access.
        let confirm_g = l[clk..].iter().position(|e| *e == Ev::PmR(PM_GRAFX)).map(|i| i + clk).expect("re-read GRAFX");
        let confirm_v = l[clk..].iter().position(|e| *e == Ev::PmR(PM_GRAFX_2712)).map(|i| i + clk).expect("re-read 2712");
        assert!(isfunc < rstn && rstn < clk && clk < confirm_g && confirm_g < confirm_v && confirm_v < first_v3d);
        assert_eq!(rep.grafx_before, 0x1000);
        assert_eq!(rep.grafx_after & PM_GRAFX_ON, PM_GRAFX_ON);
    }

    #[test]
    fn grafx_already_on_is_left_alone() {
        let (log, regs, mut env) = setup();
        env.grafx = 0x1000 | PM_GRAFX_ON | PM_MRDONE;
        power_on(regs, &mut env).expect("power_on");
        assert!(grafx_writes(&log.borrow()).is_empty(), "firmware-powered island untouched (Linux: POWUP set => return)");
    }

    #[test]
    fn grafx_no_powok_is_inert_and_undone() {
        let (log, regs, mut env) = setup();
        env.grafx_powok_at = None;
        let (e, _) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e.step(), Step::Grafx);
        let l = log.borrow();
        assert!(!l.iter().any(is_v3d), "no V3D access");
        assert!(!l.iter().any(|e| matches!(e, Ev::ClkState(..) | Ev::PmW(PM_GRAFX_2712, _))));
        assert_eq!(grafx_writes(&l).len(), 5, "4 inrush steps + POWUP undo");
        assert_eq!(env.grafx & (PM_POWUP | PM_INRUSH_MASK), 0);
    }

    #[test]
    fn grafx_no_mrdone_is_inert_and_undone() {
        let (log, regs, mut env) = setup();
        env.grafx_mrdone = false;
        let (e, _) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e.step(), Step::Grafx);
        assert!(!log.borrow().iter().any(is_v3d));
        assert_eq!(env.grafx & (PM_POWUP | PM_ISPOW | PM_ISFUNC), 0);
    }

    #[test]
    fn pm_confirm_blocks_v3d_access_when_island_drops() {
        // POWOK comes up, but a re-read after the clock shows isolation back on.
        let (log, regs, mut env) = setup();
        env.grafx = 0x1000 | PM_POWUP | PM_POWOK; // firmware set POWUP but left isolation on
        let (e, _) = power_on(regs, &mut env).err().expect("must fail");
        assert_eq!(e, PowerError::Mismatch { step: Step::PmConfirm, value: 0x1000 | PM_POWUP | PM_POWOK });
        assert!(!log.borrow().iter().any(is_v3d));
    }

    #[test]
    fn grafx_poll_bounded_with_frozen_clock() {
        let (_log, regs, mut env) = setup();
        env.grafx_powok_at = None;
        env.tick = 0;
        struct Frozen(MockEnv);
        impl PowerEnv for Frozen {
            fn pm_r(&mut self, o: u32) -> u32 { self.0.pm_r(o) }
            fn pm_w(&mut self, o: u32, v: u32) { self.0.pm_w(o, v) }
            fn clk_set_state(&mut self, i: u32, on: bool) -> Option<u32> { self.0.clk_set_state(i, on) }
            fn clk_get_max_rate(&mut self, i: u32) -> Option<u32> { self.0.clk_get_max_rate(i) }
            fn clk_set_rate(&mut self, i: u32, h: u32) -> Option<u32> { self.0.clk_set_rate(i, h) }
            fn delay_us(&mut self, us: u32) { self.0.delays += us as u64; }
            fn now_us(&mut self) -> u64 { 0 }
        }
        let mut f = Frozen(env);
        let (e, _) = power_on(regs, &mut f).err().expect("must fail");
        assert_eq!(e.step(), Step::Grafx);
        assert_eq!(f.0.delays, 4 * (POWOK_TIMEOUT_US + 1));
    }

    #[test]
    fn sms_state_is_four_bits() {
        // Pi 5 boot 2026-10-26: TEE_CS read 0x50 (OLD_MODE=1, NEW_MODE=1, STATE=IDLE).
        assert_eq!(sms_state(0x50), SMS_STATE_IDLE);
        assert_eq!(sms_state(0x5d), SMS_STATE_POWER_OFF);
    }

    #[test]
    fn field_helpers() {
        assert_eq!(mmu_va_width(0x620), 32);
        assert_eq!(mmu_pa_width(0x620), 36);
        assert_eq!(ctl_ident1_qpus(0x2000_0421), 8);
        assert_eq!(hub_ident3_iprev(0x0004_0600), 6);
        assert_eq!(sms_state(SMS_CLEAR_POWER_OFF | 0xb), SMS_STATE_RESETTING);
    }
}
