//! Locking between threads.
//!
//! `Mutex` protects a value; `wait` / `wake` are the kernel's futexes under
//! it: sleep while a word still holds a value, and wake those sleeping on
//! it. An uncontended lock never enters the kernel.

use crate::{check, sys, syscall6, Errno};
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// Sleep while `word` holds `expected`. Returns at once if it doesn't
/// (EAGAIN), and early for a signal (EINTR): callers check again in a loop.
pub fn wait(word: &AtomicU32, expected: u32) -> Result<(), Errno> {
    let addr = word as *const AtomicU32 as u64;
    check(unsafe { syscall6(sys::FUTEX_WAIT, addr, expected as u64, 0, 0, 0, 0) }).map(|_| ())
}

/// Wake up to `n` threads sleeping in `wait` on `word`; returns how many.
pub fn wake(word: &AtomicU32, n: u32) -> u32 {
    let addr = word as *const AtomicU32 as u64;
    unsafe { syscall6(sys::FUTEX_WAKE, addr, n as u64, 0, 0, 0, 0).max(0) as u32 }
}

/// A lock around a `T`. Not reentrant: locking it twice from one thread
/// deadlocks, and so does a signal handler locking what the code it
/// interrupted holds.
pub struct Mutex<T> {
    /// UNLOCKED, LOCKED, or CONTENDED: locked, and someone may be sleeping
    /// on it, so unlocking must wake them.
    state: AtomicU32,
    value: UnsafeCell<T>,
}

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
const CONTENDED: u32 = 2;

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> {
        Mutex { state: AtomicU32::new(UNLOCKED), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        if self.state.compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed).is_err() {
            // Mark it contended, then sleep until it's free: whoever holds
            // it then wakes one sleeper when unlocking. When we get it this
            // way it stays marked contended even if nobody else waits,
            // which costs one needless wake call.
            while self.state.swap(CONTENDED, Ordering::Acquire) != UNLOCKED {
                let _ = wait(&self.state, CONTENDED);
            }
        }
        MutexGuard { mutex: self }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let free = self.state.compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed);
        free.is_ok().then_some(MutexGuard { mutex: self })
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.mutex.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            wake(&self.mutex.state, 1);
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.value.get() }
    }
}
