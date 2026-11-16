//! Signals (Linux numbers) and their handlers.
//!
//! By default a signal ends the process, except SIGCHLD, SIGCONT, SIGURG
//! and SIGWINCH, which are ignored. `set_handler` changes that: a handler
//! runs on the process's own stack when the signal arrives, interrupting
//! whatever was running, which resumes when the handler returns. A system
//! call that was blocked (a read, `wait`, `sleep_ms`) then fails with EINTR.
//! While a handler runs its signal is blocked, so it doesn't nest.
//!
//! A handler interrupts the program at any point, so it should only touch
//! atomics and make system calls; in particular it must not allocate (the
//! heap may be in the middle of an update).
//!
//! SIGKILL and SIGSTOP can't be caught, ignored or blocked.

use crate::{check, syscall, sys, Errno};
use core::arch::global_asm;

pub const SIGHUP: u8 = 1;
pub const SIGINT: u8 = 2;
pub const SIGQUIT: u8 = 3;
pub const SIGILL: u8 = 4;
pub const SIGTRAP: u8 = 5;
pub const SIGABRT: u8 = 6;
pub const SIGBUS: u8 = 7;
pub const SIGFPE: u8 = 8;
pub const SIGKILL: u8 = 9;
pub const SIGUSR1: u8 = 10;
pub const SIGSEGV: u8 = 11;
pub const SIGUSR2: u8 = 12;
pub const SIGPIPE: u8 = 13;
pub const SIGALRM: u8 = 14;
pub const SIGTERM: u8 = 15;
pub const SIGCHLD: u8 = 17;
pub const SIGCONT: u8 = 18;
pub const SIGSTOP: u8 = 19;
pub const SIGURG: u8 = 23;
pub const SIGWINCH: u8 = 28;
pub const MAX: u8 = 31;

const NAMES: [(u8, &str); 20] = [
    (SIGHUP, "SIGHUP"), (SIGINT, "SIGINT"), (SIGQUIT, "SIGQUIT"), (SIGILL, "SIGILL"),
    (SIGTRAP, "SIGTRAP"), (SIGABRT, "SIGABRT"), (SIGBUS, "SIGBUS"), (SIGFPE, "SIGFPE"),
    (SIGKILL, "SIGKILL"), (SIGUSR1, "SIGUSR1"), (SIGSEGV, "SIGSEGV"), (SIGUSR2, "SIGUSR2"),
    (SIGPIPE, "SIGPIPE"), (SIGALRM, "SIGALRM"), (SIGTERM, "SIGTERM"), (SIGCHLD, "SIGCHLD"),
    (SIGCONT, "SIGCONT"), (SIGSTOP, "SIGSTOP"), (SIGURG, "SIGURG"), (SIGWINCH, "SIGWINCH"),
];

pub fn name(sig: u8) -> &'static str {
    NAMES.iter().find(|&&(n, _)| n == sig).map_or("a signal", |&(_, name)| name)
}

/// "INT", "SIGINT" or "2" -> 2.
pub fn parse(s: &str) -> Option<u8> {
    if let Ok(n) = s.parse::<u8>() {
        return (1..=MAX).contains(&n).then_some(n);
    }
    let s = s.strip_prefix("SIG").unwrap_or(s);
    NAMES.iter().find(|&&(_, name)| &name[3..] == s).map(|&(n, _)| n)
}

// ------------------------------------------------------------------ sets

/// A set of signals: bit `sig - 1` for each.
pub type Set = u64;

pub const fn bit(sig: u8) -> Set {
    1 << (sig - 1)
}

const SIG_BLOCK: u64 = 0;
const SIG_UNBLOCK: u64 = 1;
const SIG_SETMASK: u64 = 2;

fn procmask(how: u64, set: Option<Set>) -> Result<Set, Errno> {
    let mut old: Set = 0;
    let set_ptr = set.as_ref().map_or(0, |s| s as *const Set as u64);
    check(unsafe { syscall(sys::SIGPROCMASK, how, set_ptr, &mut old as *mut Set as u64) })?;
    Ok(old)
}

/// The signals now blocked: they stay pending until unblocked.
pub fn blocked() -> Set {
    procmask(SIG_BLOCK, None).unwrap_or(0)
}

/// Block `set` as well; returns the old mask.
pub fn block(set: Set) -> Set {
    procmask(SIG_BLOCK, Some(set)).unwrap_or(0)
}

/// Unblock `set` (pending ones are acted on at once); returns the old mask.
pub fn unblock(set: Set) -> Set {
    procmask(SIG_UNBLOCK, Some(set)).unwrap_or(0)
}

/// Block exactly `set`; returns the old mask.
pub fn set_blocked(set: Set) -> Set {
    procmask(SIG_SETMASK, Some(set)).unwrap_or(0)
}

/// With `set` blocked meanwhile, wait for a signal that has a handler (or
/// ends the process) and return after that handler has run. The mask is
/// then back to what it was.
pub fn suspend(set: Set) {
    unsafe { syscall(sys::SIGSUSPEND, set, 0, 0) };
}

// -------------------------------------------------------------- handlers

/// The registers a signal interrupted, as saved on the stack. A handler
/// receives a pointer to them as its third argument (rdx) and may change
/// them: they are restored when it returns.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Regs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rsp: u64,
    pub rflags: u64,
}

/// What the kernel pushes on the stack to run a handler (see the kernel's
/// `syscall/signal.rs`); the handler is entered with rsp at `restorer`.
#[repr(C)]
pub struct Frame {
    pub restorer: u64,
    pub sig: u64,
    pub regs: Regs,
    /// The signal mask to go back to.
    pub mask: Set,
    pub _pad: [u64; 2],
    /// x87/SSE registers, `fxsave` layout.
    pub fpu: [u8; 512],
}

/// What a signal does.
#[derive(Clone, Copy, Debug)]
pub enum Handler {
    Default,
    Ignore,
    /// Called with the signal number.
    Call(extern "C" fn(i32)),
    /// Called with the signal number, 0, and the interrupted registers.
    WithRegs(extern "C" fn(i32, usize, *mut Regs)),
}

/// The kernel's `sigaction` layout.
#[repr(C)]
#[derive(Default)]
struct Action {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: Set,
}

const SA_RESTORER: u64 = 0x0400_0000;
/// Don't block the signal while its handler runs.
pub const SA_NODEFER: u64 = 0x4000_0000;
/// Back to the default action once the handler has been called.
pub const SA_RESETHAND: u64 = 0x8000_0000;

extern "C" {
    fn ulib_sigreturn();
}

// Where a handler returns to: hands the frame back to the kernel.
global_asm!(
    ".global ulib_sigreturn",
    "ulib_sigreturn:",
    "    mov eax, {n}",
    "    syscall",
    "    ud2",
    n = const sys::SIGRETURN,
);

/// Set what `sig` does; returns what it did before.
pub fn set_handler(sig: u8, handler: Handler) -> Result<Handler, Errno> {
    set_handler_with(sig, handler, 0, 0)
}

/// Like `set_handler`, with SA_* `flags` and `mask`, more signals to block
/// while the handler runs.
pub fn set_handler_with(sig: u8, handler: Handler, flags: u64, mask: Set) -> Result<Handler, Errno> {
    let address = match handler {
        Handler::Default => 0,
        Handler::Ignore => 1,
        Handler::Call(f) => f as usize as u64,
        Handler::WithRegs(f) => f as usize as u64,
    };
    let new = Action { handler: address, flags: flags | SA_RESTORER, restorer: ulib_sigreturn as *const () as u64, mask };
    let mut old = Action::default();
    check(unsafe {
        syscall(sys::SIGACTION, sig as u64, &new as *const Action as u64, &mut old as *mut Action as u64)
    })?;
    Ok(match old.handler {
        0 => Handler::Default,
        1 => Handler::Ignore,
        // Every handler is installed through here, so it has this type; one
        // installed as `WithRegs` reads back as `Call`, which is the same
        // address (only the arguments it looks at differ).
        f => Handler::Call(unsafe { core::mem::transmute::<usize, extern "C" fn(i32)>(f as usize) }),
    })
}
