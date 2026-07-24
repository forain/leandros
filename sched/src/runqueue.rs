//! Run queue — fixed-size array of task slots with EEVDF selection.
//!
//! Selection policy: Earliest Eligible Virtual Deadline First (EEVDF), the
//! same family as Linux ≥ 6.6's default scheduler.
//!
//!  * Every runnable task has a `vruntime` (weighted virtual runtime) and a
//!    `vdeadline` (`vruntime + slice/weight` at last renewal).
//!  * A task is **eligible** when its `vruntime` is at or below the weighted
//!    average vruntime of the runnable set — i.e. it has received no more
//!    than its fair share of CPU so far.
//!  * `pick_next` returns the eligible task with the earliest virtual
//!    deadline.
//!
//! SMP: the queue is shared by all CPUs (guarded by the `RUN_QUEUE` mutex in
//! lib.rs).  Tasks with `on_cpu.is_some()` are skipped — their register state
//! is still live on another core.

use super::task::{Pid, Task, TaskState};
pub const MAX_TASKS: usize = 256;

use alloc::boxed::Box;

pub struct RunQueue {
    pub tasks: [Option<Box<Task>>; MAX_TASKS],
    len:       usize,
}

impl RunQueue {
    pub const fn new() -> Self {
        Self { tasks: [const { None }; MAX_TASKS], len: 0 }
    }

    /// Get the number of tasks currently in the run queue.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Minimum `vruntime` over all runnable (Ready or Running) tasks.
    ///
    /// Used to place newly enqueued or freshly woken tasks so they compete
    /// fairly from "now" instead of replaying the virtual time they slept
    /// through.
    pub fn min_vruntime(&self) -> u64 {
        self.tasks.iter()
            .filter_map(|s| s.as_ref())
            .filter(|t| matches!(t.state, TaskState::Ready | TaskState::Running))
            .map(|t| t.vruntime)
            .min()
            .unwrap_or(0)
    }

    /// Insert a task into the first free slot. Returns false if the queue is full.
    ///
    /// The task is EEVDF-placed relative to the current runnable set.
    pub fn enqueue(&mut self, mut task: Box<Task>) -> bool {
        let min_vr = self.min_vruntime();
        task.place(min_vr);
        for slot in self.tasks.iter_mut() {
            if slot.is_none() {
                *slot = Some(task);
                self.len += 1;
                return true;
            }
        }
        false
    }

    /// EEVDF pick: the eligible Ready task with the earliest virtual deadline.
    ///
    /// Tasks already running on another CPU (`on_cpu.is_some()`) are skipped.
    /// Returns the slot index so the caller can track which task is active.
    pub fn pick_next(&mut self) -> Option<usize> {
        if self.len == 0 { return None; }

        // Pass 1: total weight and weighted vruntime sum of the candidates.
        // The weighted average sum_wv / sum_w is the virtual time "V" against
        // which eligibility is judged.
        let mut sum_w:  u64  = 0;
        let mut sum_wv: u128 = 0;
        for slot in self.tasks.iter() {
            if let Some(t) = slot {
                if t.state == TaskState::Ready && t.on_cpu.is_none()
                    && !super::quiesce_filtered(t.tgid, t.pid)
                {
                    sum_w  += t.weight as u64;
                    sum_wv += t.weight as u128 * t.vruntime as u128;
                }
            }
        }
        if sum_w == 0 { return None; }

        // Pass 2: earliest virtual deadline among eligible tasks.  The task
        // with the minimum vruntime is always eligible, so `best` is always
        // found; `fallback` guards against arithmetic corner cases only.
        let mut best:     Option<(usize, u64)> = None; // (idx, vdeadline)
        let mut fallback: Option<(usize, u64)> = None; // (idx, vruntime)
        for (i, slot) in self.tasks.iter().enumerate() {
            if let Some(t) = slot {
                if t.state != TaskState::Ready || t.on_cpu.is_some() { continue; }
                // Stop-the-world fork: siblings of a mid-clone_as thread
                // group stay parked until the CoW downgrade + TLB shootdown
                // are complete (see sched::quiesce_thread_group).
                if super::quiesce_filtered(t.tgid, t.pid) { continue; }
                let eligible = (t.vruntime as u128) * (sum_w as u128) <= sum_wv;
                if eligible && best.map_or(true, |(_, d)| t.vdeadline < d) {
                    best = Some((i, t.vdeadline));
                }
                if fallback.map_or(true, |(_, v)| t.vruntime < v) {
                    fallback = Some((i, t.vruntime));
                }
            }
        }
        best.or(fallback).map(|(i, _)| i)
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Task> {
        self.tasks[idx].as_mut().map(|boxed_task| boxed_task.as_mut())
    }

    pub fn get(&self, idx: usize) -> Option<&Task> {
        self.tasks[idx].as_ref().map(|boxed_task| boxed_task.as_ref())
    }

    pub fn find_pid(&self, pid: Pid) -> Option<&Task> {
        for slot in &self.tasks {
            if let Some(task) = slot {
                if task.pid == pid {
                    return Some(task);
                }
            }
        }
        None
    }

    pub fn find_pid_mut(&mut self, pid: Pid) -> Option<&mut Task> {
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.pid == pid {
                    return Some(task);
                }
            }
        }
        None
    }

    pub fn find_pid_idx(&self, pid: Pid) -> Option<usize> {
        self.tasks.iter().position(|s| {
            s.as_ref().map(|t| t.pid == pid).unwrap_or(false)
        })
    }

    /// Block the task with `pid`, recording the port it is waiting on.
    pub fn block_on_port(&mut self, pid: Pid, port: u32) {
        self.block_on_port_until(pid, port, u64::MAX);
    }

    /// Block the task with `pid` on `port`, recording its wake deadline
    /// (absolute ticks; `u64::MAX` = none). Set atomically with `state`/
    /// `blocked_on` so the poll-deadline tick sees a consistent snapshot.
    pub fn block_on_port_until(&mut self, pid: Pid, port: u32, deadline: u64) {
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.pid == pid {
                    task.state         = TaskState::Blocked;
                    task.blocked_on    = Some(port);
                    task.poll_deadline = deadline;
                    return;
                }
            }
        }
    }

    /// Wake all tasks blocked on `port`.  Returns the number woken so the
    /// caller can kick an idle CPU when work became available.
    pub fn unblock_port(&mut self, port: u32) -> usize {
        let min_vr = self.min_vruntime();
        let mut woken = 0;
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked {
                    task.state         = TaskState::Ready;
                    task.blocked_on    = None;
                    task.poll_deadline = u64::MAX;
                    task.place(min_vr);
                    woken += 1;
                }
            }
        }
        woken
    }

    /// Poll-deadline tick service: wake every task on `port` whose
    /// `poll_deadline` is due (`<= now`) or, when `timerfd_due`, all of them
    /// (a timerfd expired — every parked poller must re-probe it). Returns
    /// `(earliest_remaining_deadline, woken)` so the caller can republish the
    /// exact next deadline (no single-global clobber) and kick an idle CPU.
    /// The whole scan runs under one RUN_QUEUE hold, so a woken task can only
    /// re-register its next deadline after this returns — the recomputed
    /// minimum can never be stale-clobbered by a concurrent register.
    pub fn wake_due_poll_deadlines(&mut self, port: u32, now: u64, timerfd_due: bool)
        -> (u64, usize)
    {
        let min_vr = self.min_vruntime();
        let mut new_min = u64::MAX;
        let mut woken = 0;
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.state != TaskState::Blocked { continue; }
                let is_poll  = task.blocked_on == Some(port);
                // Timed futex waiters (see sched::futex_wait) register a
                // `poll_deadline` and must be released at timeout too — they are
                // Blocked on `blocked_futex`, not the poll channel. `timerfd_due`
                // is a poll-channel concept (an expired timerfd every poller must
                // re-probe) and never mass-wakes futex waiters.
                let is_futex = task.blocked_futex != 0;
                if !is_poll && !is_futex { continue; }
                if (timerfd_due && is_poll) || task.poll_deadline <= now {
                    task.state         = TaskState::Ready;
                    if is_poll  { task.blocked_on    = None; }
                    if is_futex { task.blocked_futex = 0;    }
                    task.poll_deadline = u64::MAX;
                    task.place(min_vr);
                    woken += 1;
                } else if task.poll_deadline < new_min {
                    new_min = task.poll_deadline;
                }
            }
        }
        (new_min, woken)
    }

    /// Mark a task as Zombie (terminal; will not be scheduled again).
    pub fn mark_zombie(&mut self, pid: Pid) {
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.pid == pid {
                    task.state = TaskState::Zombie;
                    return;
                }
            }
        }
    }

    /// Remove the task at `idx` from the run queue and return it so the caller
    /// can free its resources.  Decrements the task count.
    pub fn remove(&mut self, idx: usize) -> Option<Box<Task>> {
        let t = self.tasks[idx].take();
        if t.is_some() {
            self.len = self.len.saturating_sub(1);
        }
        t
    }
}
