//! Host test of the NVIDIA GSP kexec unload (drivers/nvidia/src/kexec_sm.rs)
//! with a fake RM transport: a real worker thread whose RM calls are
//! recorded, and that can be made to hang in any phase.

#[path = "../../src/kexec_sm.rs"]
pub mod kexec_sm;

#[cfg(test)]
mod tests {
    use super::kexec_sm::*;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct Shared {
        calls: Mutex<Vec<&'static str>>,
        phase: AtomicU8,
    }

    struct FakeRm {
        s: Arc<Shared>,
        hang_in: Option<Phase>,
    }

    impl FakeRm {
        fn call(&mut self, name: &'static str, p: Phase) {
            self.s.calls.lock().unwrap().push(name);
            if self.hang_in == Some(p) {
                // A hung RM call: never returns (the thread is leaked).
                loop { std::thread::park(); }
            }
        }
    }

    impl Rm for FakeRm {
        fn disable_adapter(&mut self) { self.call("rm_disable_adapter", Phase::Disabling) }
        fn stop_irq(&mut self) { self.call("stop_irq", Phase::StoppingIrq) }
        fn shutdown_adapter(&mut self) { self.call("rm_shutdown_adapter", Phase::Unloading) }
        fn set_phase(&mut self, p: Phase) { self.s.phase.store(p as u8, Ordering::Release) }
    }

    struct FakeHost {
        s: Arc<Shared>,
        hang_in: Option<Phase>,
        spawn_ok: bool,
        wpr2: Option<bool>,
        t0: Instant,
        started: u32,
    }

    impl FakeHost {
        fn new() -> Self {
            FakeHost { s: Arc::default(), hang_in: None, spawn_ok: true, wpr2: Some(false), t0: Instant::now(), started: 0 }
        }
        fn calls(&self) -> Vec<&'static str> { self.s.calls.lock().unwrap().clone() }
        fn count(&self, name: &str) -> usize { self.calls().iter().filter(|c| **c == name).count() }
    }

    impl Host for FakeHost {
        fn start_worker(&mut self) -> bool {
            if !self.spawn_ok { return false; }
            self.started += 1;
            let mut rm = FakeRm { s: self.s.clone(), hang_in: self.hang_in };
            std::thread::spawn(move || worker(&mut rm));
            true
        }
        fn phase(&self) -> Phase { Phase::from_u8(self.s.phase.load(Ordering::Acquire)) }
        fn now_ms(&self) -> u64 { self.t0.elapsed().as_millis() as u64 }
        fn sleep_ms(&mut self, ms: u64) { std::thread::sleep(Duration::from_millis(ms)) }
        fn wpr2_up(&mut self) -> Option<bool> { self.wpr2 }
    }

    fn cfg(timeout_ms: u64) -> Cfg { Cfg { enabled: true, gsp_up: true, timeout_ms } }

    #[test]
    fn unload_runs_in_linux_order_exactly_once() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        let o = run(&once, &cfg(2000), &mut h);
        assert_eq!(o, Outcome::Unloaded { wpr2_checked: true });
        assert!(!o.wants_flr());
        assert_eq!(h.calls(), ["rm_disable_adapter", "stop_irq", "rm_shutdown_adapter"]);
        // A second kexec attempt (e.g. exec failed and the user retries).
        assert_eq!(run(&once, &cfg(2000), &mut h), Outcome::AlreadyRan);
        assert_eq!(h.count("rm_shutdown_adapter"), 1, "GSP memory would be freed twice");
        assert_eq!(h.started, 1);
    }

    #[test]
    fn hang_in_gsp_unload_times_out_and_asks_for_flr() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        h.hang_in = Some(Phase::Unloading);
        let t = Instant::now();
        let o = run(&once, &cfg(100), &mut h);
        let el = t.elapsed().as_millis();
        assert_eq!(o, Outcome::Timeout(Phase::Unloading));
        assert!(o.wants_flr());
        assert!(el >= 100 && el < 100 + 500, "wait not bounded: {el} ms");
        // The hung worker is abandoned; nothing ever starts a second unload.
        assert_eq!(run(&once, &cfg(100), &mut h), Outcome::AlreadyRan);
        assert_eq!(h.count("rm_shutdown_adapter"), 1);
        assert_eq!(h.started, 1);
    }

    #[test]
    fn hang_in_disable_never_reaches_unload() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        h.hang_in = Some(Phase::Disabling);
        let o = run(&once, &cfg(50), &mut h);
        assert_eq!(o, Outcome::Timeout(Phase::Disabling));
        assert!(o.wants_flr());
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(h.calls(), ["rm_disable_adapter"]);
    }

    #[test]
    fn wpr2_left_up_asks_for_flr() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        h.wpr2 = Some(true);
        let o = run(&once, &cfg(2000), &mut h);
        assert_eq!(o, Outcome::Wpr2StillUp);
        assert!(o.wants_flr());
    }

    #[test]
    fn unreadable_wpr2_is_not_a_failure() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        h.wpr2 = None;
        let o = run(&once, &cfg(2000), &mut h);
        assert_eq!(o, Outcome::Unloaded { wpr2_checked: false });
        assert!(!o.wants_flr());
    }

    #[test]
    fn worker_spawn_failure_falls_back_to_flr() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        h.spawn_ok = false;
        let o = run(&once, &cfg(2000), &mut h);
        assert_eq!(o, Outcome::WorkerFailed);
        assert!(o.wants_flr());
        assert!(h.calls().is_empty());
    }

    #[test]
    fn knob_off_or_gsp_down_touches_nothing() {
        let once = AtomicBool::new(false);
        let mut h = FakeHost::new();
        let off = Cfg { enabled: false, gsp_up: true, timeout_ms: 100 };
        assert_eq!(run(&once, &off, &mut h), Outcome::Disabled);
        let down = Cfg { enabled: true, gsp_up: false, timeout_ms: 100 };
        assert_eq!(run(&once, &down, &mut h), Outcome::NotRunning);
        assert!(!Outcome::Disabled.wants_flr() && !Outcome::NotRunning.wants_flr());
        assert!(h.calls().is_empty());
        assert_eq!(h.started, 0);
        // Neither consumed the guard: a later enabled call still unloads.
        assert_eq!(run(&once, &cfg(2000), &mut h), Outcome::Unloaded { wpr2_checked: true });
    }

    #[test]
    fn phase_roundtrip() {
        for p in [Phase::Idle, Phase::Disabling, Phase::StoppingIrq, Phase::Unloading, Phase::Done] {
            assert_eq!(Phase::from_u8(p as u8), p);
        }
        assert_eq!(Phase::from_u8(99), Phase::Idle);
    }
}
