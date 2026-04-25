//! Run queue — fixed-size array of task slots with round-robin selection.
//!
//! Future: replace with a red-black tree keyed on virtual runtime (à la CFS).

use super::task::{Pid, Task, TaskState};
pub const MAX_TASKS: usize = 256;

use alloc::boxed::Box;
use alloc::format;

pub struct RunQueue {
    pub tasks: [Option<Box<Task>>; MAX_TASKS],
    len:       usize,
    cursor:    usize,
}

impl RunQueue {
    pub const fn new() -> Self {
        Self { tasks: [const { None }; MAX_TASKS], len: 0, cursor: 0 }
    }

    /// Get the number of tasks currently in the run queue.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Insert a task into the first free slot. Returns false if the queue is full.
    pub fn enqueue(&mut self, task: Box<Task>) -> bool {
        for (idx, slot) in self.tasks.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(task);
                self.len += 1;
                return true;
            }
        }
        false
    }

    /// Pick the next Ready task using round-robin scheduling.
    ///
    /// Starts from the current cursor position and finds the next Ready task.
    /// This ensures fair scheduling and prevents any single task from starving.
    /// Returns the slot index so the caller can track which task is active.
    pub fn pick_next(&mut self) -> Option<usize> {
        if self.len == 0 { return None; }

        // First, try from cursor to end
        for i in self.cursor..MAX_TASKS {
            if let Some(ref task) = self.tasks[i] {
                if task.state == TaskState::Ready {
                    self.cursor = (i + 1) % MAX_TASKS;
                    return Some(i);
                }
            }
        }

        // Then try from start to cursor
        for i in 0..self.cursor {
            if let Some(ref task) = self.tasks[i] {
                if task.state == TaskState::Ready {
                    self.cursor = (i + 1) % MAX_TASKS;
                    return Some(i);
                }
            }
        }

        None
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
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.pid == pid {
                    task.state      = TaskState::Blocked;
                    task.blocked_on = Some(port);
                    return;
                }
            }
        }
    }

    /// Wake all tasks blocked on `port`.
    pub fn unblock_port(&mut self, port: u32) {
        for slot in &mut self.tasks {
            if let Some(task) = slot {
                if task.blocked_on == Some(port) && task.state == TaskState::Blocked {
                    task.state      = TaskState::Ready;
                    task.blocked_on = None;
                }
            }
        }
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
