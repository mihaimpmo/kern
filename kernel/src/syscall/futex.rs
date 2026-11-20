//! Futexes: sleeping on a word of user memory, the building block for
//! locks, condition variables and `join` in user space.
//!
//! `futex_wait(addr, expected)` sleeps while the u32 at `addr` holds
//! `expected`; `futex_wake(addr, n)` wakes up to `n` threads sleeping on
//! `addr`. Locks stay in user space as long as nobody has to wait: a thread
//! only makes these calls when it finds a lock taken, or knows someone
//! sleeps on it.
//!
//! Futexes are private to a process: the key is (process, address). A queue
//! exists only while somebody waits on it. The check of the value and going
//! to sleep happen under the table's lock, which `futex_wake` takes too, so
//! a wake can't slip in between (the waker changes the word first).

use super::{copy_from_user, Errno, SysResult, EAGAIN, EINTR, EINVAL};
use crate::sync::SpinLock;
use crate::task::{self, Pid, WaitQueue};
use alloc::{collections::BTreeMap, sync::Arc};

/// (tgid, address) -> the threads waiting there.
static TABLE: SpinLock<BTreeMap<(Pid, u64), Arc<WaitQueue>>> = SpinLock::new(BTreeMap::new());

pub fn wait(addr: u64, expected: u64) -> SysResult {
    if addr % 4 != 0 {
        return Err(EINVAL);
    }
    let key = (task::current_pid(), addr);
    let mut table = TABLE.lock();
    let mut word = [0u8; 4];
    copy_from_user(&mut word, addr)?;
    if u32::from_le_bytes(word) as u64 != expected {
        return Err(EAGAIN);
    }
    let queue = table.entry(key).or_insert_with(|| Arc::new(WaitQueue::new())).clone();
    let woken = queue.wait_interruptible(table).map(drop);
    forget_if_unused(key, &queue);
    woken.map(|()| 0).map_err(|_| EINTR)
}

/// Wake up to `n` threads waiting on the calling process's `addr`; returns
/// how many.
pub fn wake(addr: u64, n: u64) -> u64 {
    let key = (task::current_pid(), addr);
    let table = TABLE.lock();
    let Some(queue) = table.get(&key).cloned() else { return 0 };
    // With the table locked, so a waiter can't be between checking the word
    // and joining the queue.
    let woken = queue.wake(n);
    drop(table);
    forget_if_unused(key, &queue);
    woken
}

pub fn wake_syscall(addr: u64, n: u64) -> SysResult {
    if addr % 4 != 0 {
        return Err::<u64, Errno>(EINVAL);
    }
    Ok(wake(addr, n))
}

/// Drop the queue for `key` if nobody waits on it. Waiters join a queue
/// only with the table locked, so it can't gain one meanwhile.
fn forget_if_unused(key: (Pid, u64), queue: &Arc<WaitQueue>) {
    let mut table = TABLE.lock();
    if queue.is_empty() && table.get(&key).map_or(false, |q| Arc::ptr_eq(q, queue)) {
        table.remove(&key);
    }
}
