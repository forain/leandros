//! Pure decisions behind the shutdown-time write-cache flush (host-tested in
//! `drivers/flush-tests`).

/// What an NVMe namespace flush must do.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum NvmeFlushPlan {
    /// Nothing was written or the controller has no volatile cache.
    Skip,
    /// Controller already took CC.SHN (shutdown notification): its cache is
    /// committed by definition and the queues are gone, so a Flush command
    /// cannot be sent and must not be reported as a failure.
    AlreadyCommitted,
    /// Send the Flush command.
    Send,
}

pub fn nvme_flush_plan(read_only: bool, vwc: bool, shut_down: bool) -> NvmeFlushPlan {
    if read_only || !vwc { NvmeFlushPlan::Skip }
    else if shut_down { NvmeFlushPlan::AlreadyCommitted }
    else { NvmeFlushPlan::Send }
}

/// Outcome of one disk's flush.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum FlushResult { Ok, Unsupported, Failed }

/// Per-class counts for the shutdown report.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct FlushTally { pub ok: usize, pub unsupported: usize, pub failed: usize }

impl FlushTally {
    pub fn add(&mut self, r: FlushResult) {
        match r {
            FlushResult::Ok => self.ok += 1,
            FlushResult::Unsupported => self.unsupported += 1,
            FlushResult::Failed => self.failed += 1,
        }
    }
}
