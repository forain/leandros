#[path = "../../src/flush_logic.rs"]
pub mod logic;

#[cfg(test)]
mod tests {
    use super::logic::*;

    #[test]
    fn nvme_after_shutdown_is_committed_not_a_failure() {
        // kexec/poweroff: nvme::shutdown_all runs in the prepare phase, the
        // disk flush later in machine_prepare. Used to report "1 failed".
        assert_eq!(nvme_flush_plan(false, true, true), NvmeFlushPlan::AlreadyCommitted);
    }

    #[test]
    fn nvme_plans() {
        assert_eq!(nvme_flush_plan(false, true, false), NvmeFlushPlan::Send);
        assert_eq!(nvme_flush_plan(true, true, false), NvmeFlushPlan::Skip);
        assert_eq!(nvme_flush_plan(false, false, true), NvmeFlushPlan::Skip);
        assert_eq!(nvme_flush_plan(true, true, true), NvmeFlushPlan::Skip);
    }

    #[test]
    fn tally_separates_unsupported_from_failed() {
        let mut t = FlushTally::default();
        for r in [FlushResult::Ok, FlushResult::Unsupported, FlushResult::Ok, FlushResult::Failed] { t.add(r); }
        assert_eq!(t, FlushTally { ok: 2, unsupported: 1, failed: 1 });
    }
}
