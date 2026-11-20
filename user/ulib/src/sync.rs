//! Locking between threads.
//!
//! `Mutex` protects a value; `wait` / `wake` are the primitive under it:
//! sleep while a word still holds a value, and wake those sleeping on it.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

/// Sleep while `word` holds `expected`. May return early: check again.
pub fn wait(word: &AtomicU32, expected: u32) {
    if word.load(Ordering::Acquire) == expected {
        crate::yield_now();
    }
}

/// Wake up to `n` threads sleeping in `wait` on `word`.
pub fn wake(_word: &AtomicU32, _n: u32) {}

/// A lock around a `T`. Not reentrant: locking it twice from one thread
/// deadlocks, and so does a signal handler locking what the code it
/// interrupted holds.
pub struct Mutex<T> {
    /// 0: unlocked, 1: locked.
    state: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Mutex<T> {
        Mutex { state: AtomicU32::new(0), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        while self.state.swap(1, Ordering::Acquire) != 0 {
            wait(&self.state, 1);
        }
        MutexGuard { mutex: self }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        (self.state.swap(1, Ordering::Acquire) == 0).then_some(MutexGuard { mutex: self })
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.state.store(0, Ordering::Release);
        wake(&self.mutex.state, 1);
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
