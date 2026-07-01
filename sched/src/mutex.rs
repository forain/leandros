//! Mutex implementation using futexes.
//!
//! This provides the basic mutex operations that pthreads expect:
//! - mutex_init
//! - mutex_lock
//! - mutex_trylock
//! - mutex_unlock
//! - mutex_destroy

use super::futex::{futex_wait, futex_wake};
use core::sync::atomic::{AtomicI32, Ordering};

/// A simple mutex implementation using futexes
#[repr(C)]
pub struct FutexMutex {
    /// 0 = unlocked, 1 = locked
    lock: AtomicI32,
}

impl FutexMutex {
    /// Create a new unlocked mutex
    pub const fn new() -> Self {
        Self {
            lock: AtomicI32::new(0),
        }
    }

    /// Lock the mutex, blocking if necessary
    pub fn lock(&self) {
        // Try to acquire the lock
        if self.lock.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return;
        }

        // If we failed, we need to wait
        loop {
            // Wait on the futex
            let result = futex_wait(&self.lock as *const AtomicI32 as usize, 0);
            if result == 0 {
                // Try to acquire the lock again
                if self.lock.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return;
                }
                // If we failed, we need to go back to waiting
            } else {
                // futex_wait failed, try again
                if self.lock.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    return;
                }
            }
        }
    }

    /// Try to lock the mutex without blocking
    pub fn try_lock(&self) -> bool {
        self.lock.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    /// Unlock the mutex
    pub fn unlock(&self) {
        // Release the lock
        self.lock.store(0, Ordering::Release);
        
        // Wake up one waiter
        futex_wake(&self.lock as *const AtomicI32 as usize, 1);
    }

    /// Destroy the mutex (no-op in this simple implementation)
    pub fn destroy(&self) {
        // In a real implementation, we might want to do cleanup here
        // For now, we just do nothing
    }
}

/// A condition variable implementation using futexes
#[repr(C)]
pub struct FutexCondvar {
    /// Condition variable state - simple implementation
    waiters: AtomicI32,
}

impl FutexCondvar {
    /// Create a new condition variable
    pub const fn new() -> Self {
        Self {
            waiters: AtomicI32::new(0),
        }
    }

    /// Wait on the condition variable
    pub fn wait(&self, mutex: &FutexMutex) {
        // Increment waiters count
        self.waiters.fetch_add(1, Ordering::Acquire);
        
        // Unlock the mutex
        mutex.unlock();
        
        // Wait on futex
        let result = futex_wait(&self.waiters as *const AtomicI32 as usize, 0);
        if result != 0 {
            // Handle error
        }
        
        // Reacquire the mutex
        mutex.lock();
        
        // Decrement waiters count
        self.waiters.fetch_sub(1, Ordering::Release);
    }

    /// Signal one waiting thread
    pub fn signal(&self) {
        // Wake up one waiter
        futex_wake(&self.waiters as *const AtomicI32 as usize, 1);
    }

    /// Signal all waiting threads
    pub fn broadcast(&self) {
        // Wake up all waiters
        futex_wake(&self.waiters as *const AtomicI32 as usize, u32::MAX);
    }
}