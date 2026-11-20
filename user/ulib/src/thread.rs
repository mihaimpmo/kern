//! Threads: more than one flow of control in a process, sharing its memory,
//! files and signal handlers.
//!
//! `spawn` runs a closure on a new thread with its own stack (mapped with
//! `mmap`, with a guard page below it) and `join` waits for it and returns
//! what the closure returned. A panic or `exit` in any thread ends the
//! whole process; `exit_thread` ends only the calling one.
//!
//! Dropping a `JoinHandle` without joining detaches the thread: it runs on,
//! and its stack is never freed.

use crate::{check, sys, syscall6, Errno};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

pub type Tid = u64;

/// Stack size for new threads. Pages are only allocated when touched.
pub const STACK_SIZE: usize = 1 << 20;
const GUARD: usize = 4096;

/// The calling thread's id. The first thread's id is the process's pid.
pub fn id() -> Tid {
    unsafe { syscall6(sys::GETTID, 0, 0, 0, 0, 0, 0) as Tid }
}

/// End the calling thread. If it is the last one, the process ends, with
/// `code` as its exit code.
pub fn exit_thread(code: i32) -> ! {
    unsafe { syscall6(sys::THREAD_EXIT, code as i64 as u64, 0, 0, 0, 0, 0) };
    unreachable!("thread_exit returned")
}

/// Point the FS segment at `addr`, for this thread only: `fs:[n]` then reads
/// `addr + n`. Thread-local storage is built on this.
pub fn set_fs_base(addr: u64) -> Result<(), Errno> {
    check(unsafe { syscall6(sys::SET_FS_BASE, addr, 0, 0, 0, 0, 0) }).map(|_| ())
}

/// Read the u64 at `fs:[offset]`.
///
/// # Safety
/// FS must point at memory valid for that read (see `set_fs_base`).
pub unsafe fn read_fs(offset: u64) -> u64 {
    let value: u64;
    core::arch::asm!("mov {}, fs:[{}]", out(reg) value, in(reg) offset, options(nostack, readonly));
    value
}

/// Start a thread at `entry` with stack pointer `stack` (16-byte aligned),
/// `arg` in rdi and FS base `tls`. When it ends, the kernel sets the u32 at
/// `exit_word` (if not null) to 0 and wakes its futex waiters.
///
/// # Safety
/// `entry` must be code that never returns (it has no return address) and
/// ends with `exit_thread`; the stack and `exit_word` must stay valid until
/// the thread has ended.
pub unsafe fn create(entry: u64, stack: u64, arg: u64, tls: u64, exit_word: *const AtomicU32) -> Result<Tid, Errno> {
    check(syscall6(sys::THREAD_CREATE, entry, stack, arg, tls, exit_word as u64, 0))
}

struct Shared<T> {
    result: UnsafeCell<Option<T>>,
    /// 1 while the thread runs; the kernel sets it to 0 once the thread is
    /// off its stack for good.
    running: AtomicU32,
}

// `result` is written by the thread before it ends and read by the joiner
// only after `running` says it has.
unsafe impl<T: Send> Sync for Shared<T> {}

pub struct JoinHandle<T> {
    tid: Tid,
    shared: Arc<Shared<T>>,
    stack: *mut u8,
}

impl<T> JoinHandle<T> {
    pub fn tid(&self) -> Tid {
        self.tid
    }

    /// Wait for the thread to end; returns its closure's result.
    pub fn join(self) -> T {
        while self.shared.running.load(Ordering::Acquire) != 0 {
            let _ = crate::sync::wait(&self.shared.running, 1);
        }
        // Safe now: the thread will never touch its stack or `shared` again.
        unsafe {
            let _ = crate::munmap(self.stack, STACK_SIZE + GUARD);
            let result = (*self.shared.result.get()).take();
            core::mem::forget(self); // its stack is gone already
            result.expect("thread ended without a result")
        }
    }
}

impl<T> Drop for JoinHandle<T> {
    /// Detach: the thread may still be using `shared` and its stack.
    fn drop(&mut self) {
        core::mem::forget(self.shared.clone());
    }
}

/// Run `f` on a new thread.
pub fn spawn<F, T>(f: F) -> Result<JoinHandle<T>, Errno>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let shared = Arc::new(Shared { result: UnsafeCell::new(None), running: AtomicU32::new(1) });
    let theirs = shared.clone();
    let run: Box<dyn FnOnce()> = Box::new(move || {
        let value = f();
        unsafe { *theirs.result.get() = Some(value) };
    });
    let start = Box::into_raw(Box::new(run));

    let stack = crate::mmap(STACK_SIZE + GUARD, crate::prot::READ | crate::prot::WRITE)?;
    let fail = |e| unsafe {
        let _ = crate::munmap(stack, STACK_SIZE + GUARD);
        drop(Box::from_raw(start));
        e
    };
    crate::mprotect(stack, GUARD, crate::prot::NONE).map_err(fail)?;
    let top = stack as u64 + (STACK_SIZE + GUARD) as u64;
    let tid = unsafe { create(ulib_thread_entry as *const () as u64, top, start as u64, 0, &shared.running) }
        .map_err(fail)?;
    Ok(JoinHandle { tid, shared, stack })
}

extern "C" {
    fn ulib_thread_entry();
}

// A new thread starts here with rsp at the top of its stack and the boxed
// closure in rdi; the call makes the frame look like any other call's.
global_asm!(
    ".global ulib_thread_entry",
    "ulib_thread_entry:",
    "    xor ebp, ebp",
    "    and rsp, -16",
    "    call ulib_thread_main",
    "    ud2",
);

#[no_mangle]
extern "C" fn ulib_thread_main(start: *mut Box<dyn FnOnce()>) -> ! {
    let run = unsafe { Box::from_raw(start) };
    run();
    exit_thread(0)
}
