//! Signal handlers: installing them, the signal mask, and running a handler
//! on the user stack.
//!
//! To run a handler, the kernel pushes a frame on the user stack (below the
//! 128-byte red zone), aimed so the handler starts as if called:
//!
//!   rsp ->  restorer     return address: calls sigreturn
//!           sig
//!           registers    rax rbx rcx rdx rsi rdi rbp r8-r15 rip rsp rflags
//!           mask         to restore when the handler returns
//!           2 words      padding
//!           fpu          512 bytes, fxsave layout, 16-byte aligned
//!
//! The handler gets rdi = sig, rsi = 0 (no siginfo yet) and rdx = a pointer
//! to the saved registers, which it may change. When it returns into the
//! restorer, `sigreturn` finds the frame 8 bytes below the stack pointer and
//! puts everything back. `ulib::signal::Frame` mirrors this layout.

use super::{check_user, copy_from_user, copy_to_user, Errno, SysResult, SyscallFrame, EINTR, EINVAL};
use crate::arch::fpu::FpuState;
use crate::arch::interrupts::InterruptFrame;
use crate::mm::vmm::USER_END;
use crate::task::{self, signal::*, ExitStatus};

const RED_ZONE: u64 = 128;
const REGS_AT: usize = 2; // in words
const MASK_AT: usize = REGS_AT + 18;
const FPU_AT: usize = 184; // in bytes; (MASK_AT + 3) * 8
const FRAME_BYTES: usize = FPU_AT + 512;

/// rflags bits a handler (or sigreturn) may set: the arithmetic flags,
/// TF and DF. Never IOPL, AC, ...
const USER_FLAGS: u64 = 0xdd5;
const FLAG_TF: u64 = 0x100;
const FLAG_DF: u64 = 0x400;

/// The user registers, in signal-frame order.
struct UserRegs {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    rip: u64,
    rsp: u64,
    rflags: u64,
}

impl UserRegs {
    fn words(&self) -> [u64; 18] {
        [
            self.rax, self.rbx, self.rcx, self.rdx, self.rsi, self.rdi, self.rbp, self.r8, self.r9, self.r10,
            self.r11, self.r12, self.r13, self.r14, self.r15, self.rip, self.rsp, self.rflags,
        ]
    }

    fn from_words(w: &[u64]) -> UserRegs {
        UserRegs {
            rax: w[0], rbx: w[1], rcx: w[2], rdx: w[3], rsi: w[4], rdi: w[5], rbp: w[6], r8: w[7], r9: w[8],
            r10: w[9], r11: w[10], r12: w[11], r13: w[12], r14: w[13], r15: w[14], rip: w[15], rsp: w[16],
            rflags: w[17],
        }
    }
}

/// A saved user context: the system-call frame or an interrupt frame.
trait Frame {
    fn regs(&self) -> UserRegs;
    fn set_regs(&mut self, r: &UserRegs);
}

macro_rules! frame {
    ($frame:ty) => {
        impl Frame for $frame {
            fn regs(&self) -> UserRegs {
                let f = self;
                UserRegs {
                    rax: f.rax, rbx: f.rbx, rcx: f.rcx, rdx: f.rdx, rsi: f.rsi, rdi: f.rdi, rbp: f.rbp,
                    r8: f.r8, r9: f.r9, r10: f.r10, r11: f.r11, r12: f.r12, r13: f.r13, r14: f.r14,
                    r15: f.r15, rip: f.rip, rsp: f.rsp, rflags: f.rflags,
                }
            }

            fn set_regs(&mut self, r: &UserRegs) {
                let f = self;
                (f.rax, f.rbx, f.rcx, f.rdx, f.rsi, f.rdi, f.rbp) = (r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rbp);
                (f.r8, f.r9, f.r10, f.r11, f.r12, f.r13, f.r14, f.r15) =
                    (r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15);
                (f.rip, f.rsp, f.rflags) = (r.rip, r.rsp, r.rflags);
            }
        }
    };
}

frame!(SyscallFrame);
frame!(InterruptFrame);

fn word(bytes: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap())
}

fn kill_with_segv(why: &str, at: u64) -> ! {
    kprintln!("\x1b[91m[kernel] process {} killed: {} at {:#x}\x1b[0m", task::current_pid(), why, at);
    task::exit_current(ExitStatus::Killed(SIGSEGV));
}

/// On the way back to user mode: act on the next pending signal. Returns
/// whether `regs` now lead into a handler.
fn deliver(regs: &mut UserRegs) -> bool {
    // Ends the process for a signal whose action is to.
    let Some((sig, action, restore_mask)) = task::next_signal() else { return false };

    // A fixed buffer, not the heap: if the process dies below, nothing on
    // this stack is ever dropped.
    let mut frame = [0u8; FRAME_BYTES];
    let words = [action.restorer, sig as u64].into_iter().chain(regs.words()).chain([restore_mask]);
    for (i, w) in words.enumerate() {
        frame[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    frame[FPU_AT..].copy_from_slice(&FpuState::capture());

    // 16-byte aligned plus 8, like a stack right after a call; that puts
    // the FPU area on a 16-byte boundary.
    let at = (regs.rsp.wrapping_sub(RED_ZONE + FRAME_BYTES as u64) & !0xf).wrapping_sub(8);
    if at >= USER_END || copy_to_user(at, &frame).is_err() {
        kill_with_segv("no room on the stack for a signal frame", at);
    }
    regs.rip = action.handler;
    regs.rsp = at;
    regs.rdi = sig as u64;
    regs.rsi = 0;
    regs.rdx = at + REGS_AT as u64 * 8;
    regs.rax = 0;
    regs.rflags &= !(FLAG_DF | FLAG_TF); // the ABI wants DF clear on entry
    true
}

fn deliver_to(f: &mut impl Frame) {
    let mut regs = f.regs();
    if deliver(&mut regs) {
        f.set_regs(&regs);
    }
}

pub fn deliver_from_syscall(f: &mut SyscallFrame) {
    deliver_to(f);
}

/// Called at the end of every interrupt or exception from user mode, which
/// is what stops (or diverts) a process that never makes a system call.
pub fn deliver_from_interrupt(f: &mut InterruptFrame) {
    deliver_to(f);
}

/// Back from a handler: restore what `deliver` saved. A frame that can't be
/// read, or would resume outside user space, ends the process.
pub fn sigreturn(f: &mut SyscallFrame) {
    let at = f.rsp.wrapping_sub(8);
    let mut frame = [0u8; FRAME_BYTES];
    if copy_from_user(&mut frame, at).is_err() {
        kill_with_segv("bad signal frame", at);
    }
    let words: [u64; 18] = core::array::from_fn(|i| word(&frame, REGS_AT + i));
    let mut regs = UserRegs::from_words(&words);
    if regs.rip >= USER_END || regs.rsp >= USER_END {
        kill_with_segv("signal frame returns outside user space", at);
    }
    regs.rflags = (regs.rflags & USER_FLAGS) | 0x202;
    task::with_signals(|s| s.set_blocked(word(&frame, MASK_AT)));
    FpuState::load_user(frame[FPU_AT..].try_into().unwrap());
    f.set_regs(&regs);
}

pub fn sigaction(sig: u64, act: u64, old: u64) -> SysResult {
    if sig == 0 || sig > MAX as u64 {
        return Err(EINVAL);
    }
    let sig = sig as u8;
    let new = if act != 0 {
        let mut raw = [0u8; 32];
        copy_from_user(&mut raw, act)?;
        let a = Action { handler: word(&raw, 0), flags: word(&raw, 1), restorer: word(&raw, 2), mask: word(&raw, 3) };
        if !catchable(sig) || a.flags & !SA_SUPPORTED != 0 {
            return Err(EINVAL);
        }
        let reachable = a.handler < USER_END && a.restorer < USER_END;
        if a.is_handler() && (a.flags & SA_RESTORER == 0 || !reachable) {
            return Err(EINVAL);
        }
        Some(a)
    } else {
        None
    };
    if old != 0 {
        check_user(old, 32, true)?;
    }
    let prev = task::signal_action(sig, new);
    if old != 0 {
        let mut raw = [0u8; 32];
        for (i, w) in [prev.handler, prev.flags, prev.restorer, prev.mask].iter().enumerate() {
            raw[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        copy_to_user(old, &raw)?;
    }
    Ok(0)
}

pub fn sigprocmask(how: u64, set: u64, old: u64) -> SysResult {
    let change = if set != 0 {
        if how > SIG_SETMASK {
            return Err(EINVAL);
        }
        let mut raw = [0u8; 8];
        copy_from_user(&mut raw, set)?;
        Some(u64::from_le_bytes(raw))
    } else {
        None
    };
    if old != 0 {
        check_user(old, 8, true)?;
    }
    let prev = task::with_signals(|s| {
        let prev = s.blocked();
        if let Some(m) = change {
            s.set_blocked(match how {
                SIG_BLOCK => prev | m,
                SIG_UNBLOCK => prev & !m,
                _ => m,
            });
        }
        prev
    });
    if old != 0 {
        copy_to_user(old, &prev.to_le_bytes())?;
    }
    Ok(0)
}

pub fn sigsuspend(mask: u64) -> SysResult {
    task::sigsuspend(mask);
    Err::<u64, Errno>(EINTR)
}
