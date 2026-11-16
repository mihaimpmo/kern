//! Signal numbers and a task's signal state: what is pending, what is
//! blocked, and what each signal does (its action).
//!
//! A signal is *posted* to a task (`kill`, a CPU fault, a child exiting) and
//! *delivered* on its next return to user mode (`syscall::signal::deliver`):
//! the default action ends the process (or does nothing, for the few that
//! are ignored by default), and a handler runs on the user stack until it
//! returns through `sigreturn`. Blocked signals stay pending until they are
//! unblocked. SIGKILL can't be caught, ignored or blocked.
//!
//! Numbers, the action layout and the SA_* flags are Linux's.

#![allow(dead_code)] // the full list of numbers, not all used by the kernel

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
/// Can't be caught, like SIGKILL. There is no job stopping yet, so sending
/// it ends the process.
pub const SIGSTOP: u8 = 19;
pub const SIGURG: u8 = 23;
pub const SIGWINCH: u8 = 28;
pub const MAX: u8 = 31;

/// Handler values with a special meaning.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

/// The handler returns through `restorer` (required for a handler).
pub const SA_RESTORER: u64 = 0x0400_0000;
/// Don't block the signal while its handler runs.
pub const SA_NODEFER: u64 = 0x4000_0000;
/// Back to the default action once the handler has been called.
pub const SA_RESETHAND: u64 = 0x8000_0000;
pub const SA_SUPPORTED: u64 = SA_RESTORER | SA_NODEFER | SA_RESETHAND;

/// `sigprocmask` operations.
pub const SIG_BLOCK: u64 = 0;
pub const SIG_UNBLOCK: u64 = 1;
pub const SIG_SETMASK: u64 = 2;

/// The bit for `sig` in a signal set (bit 0 is signal 1).
pub const fn bit(sig: u8) -> u64 {
    1 << (sig - 1)
}

/// Signals that can never be blocked or caught.
const UNCATCHABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);

/// What one signal does; the layout of Linux's kernel `sigaction`.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    /// Blocked (in addition to the current mask) while the handler runs.
    pub mask: u64,
}

impl Action {
    pub fn is_handler(&self) -> bool {
        self.handler > SIG_IGN
    }
}

/// Whether the default action of `sig` is to do nothing (rather than end
/// the process).
fn ignored_by_default(sig: u8) -> bool {
    matches!(sig, SIGCHLD | SIGCONT | SIGURG | SIGWINCH)
}

pub fn catchable(sig: u8) -> bool {
    (1..=MAX).contains(&sig) && bit(sig) & UNCATCHABLE == 0
}

/// What delivering a signal means for the task.
pub enum Disposition {
    /// End the process with this signal.
    Terminate(u8),
    /// Run the handler. `restore_mask` is the mask to put back when it
    /// returns; the task's mask has already been set for the handler.
    Handle { sig: u8, action: Action, restore_mask: u64 },
}

pub struct Signals {
    pending: u64,
    blocked: u64,
    actions: [Action; MAX as usize],
    /// The mask to restore after `sigsuspend` has run a handler.
    pub saved_mask: Option<u64>,
}

impl Signals {
    pub const fn new() -> Signals {
        Signals { pending: 0, blocked: 0, actions: [Action { handler: 0, flags: 0, restorer: 0, mask: 0 }; MAX as usize], saved_mask: None }
    }

    fn action(&self, sig: u8) -> &Action {
        &self.actions[sig as usize - 1]
    }

    fn ignored(&self, sig: u8) -> bool {
        match self.action(sig).handler {
            SIG_IGN => true,
            SIG_DFL => ignored_by_default(sig),
            _ => false,
        }
    }

    /// Signals that would be acted on if the task returned to user mode now.
    pub fn deliverable(&self) -> u64 {
        self.pending & !self.blocked
    }

    pub fn blocked(&self) -> u64 {
        self.blocked
    }

    pub fn set_blocked(&mut self, mask: u64) {
        self.blocked = mask & !UNCATCHABLE;
    }

    /// Whether a CPU fault raising `sig` can be handed to a handler; if not,
    /// it ends the process right away.
    pub fn fault_is_caught(&self, sig: u8) -> bool {
        self.action(sig).is_handler() && self.blocked & bit(sig) == 0
    }

    /// Make `sig` pending, unless the task ignores it (an ignored signal that
    /// is blocked stays pending: the action may change before it's
    /// unblocked). Returns whether it can be delivered right away, that is,
    /// whether a sleeping task should be woken for it.
    pub fn post(&mut self, sig: u8) -> bool {
        let b = bit(sig);
        if self.blocked & b == 0 && self.ignored(sig) && b & UNCATCHABLE == 0 {
            return false;
        }
        self.pending |= b;
        self.deliverable() & b != 0
    }

    /// Install `action` for `sig` (catchable); returns the old one. Making a
    /// signal ignored discards it if pending.
    pub fn set_action(&mut self, sig: u8, action: Action) -> Action {
        let old = core::mem::replace(&mut self.actions[sig as usize - 1], action);
        if self.ignored(sig) {
            self.pending &= !bit(sig);
        }
        old
    }

    pub fn get_action(&self, sig: u8) -> Action {
        *self.action(sig)
    }

    /// A new program can't run the old one's handlers: they go back to the
    /// default. Ignored signals stay ignored, the mask and pending signals
    /// carry over (as in POSIX).
    pub fn reset_for_exec(&mut self) {
        for a in self.actions.iter_mut() {
            if a.handler != SIG_IGN {
                *a = Action::default();
            }
        }
        self.saved_mask = None;
    }

    /// Take the next signal to act on, if any. Signals whose action is to
    /// do nothing are dropped on the way. For a handler, the mask is set up
    /// for it and the one to restore afterwards is returned.
    pub fn next(&mut self) -> Option<Disposition> {
        loop {
            let ready = self.deliverable();
            if ready == 0 {
                // sigsuspend's temporary mask ends once nothing more is to be
                // delivered (a handler that ran took the mask with it).
                if let Some(mask) = self.saved_mask.take() {
                    self.blocked = mask;
                }
                return None;
            }
            let sig = ready.trailing_zeros() as u8 + 1;
            self.pending &= !bit(sig);
            let action = *self.action(sig);
            if !catchable(sig) {
                return Some(Disposition::Terminate(sig));
            }
            match action.handler {
                SIG_IGN => continue,
                SIG_DFL if ignored_by_default(sig) => continue,
                SIG_DFL => return Some(Disposition::Terminate(sig)),
                _ => {}
            }
            let restore_mask = self.saved_mask.take().unwrap_or(self.blocked);
            let mut during = self.blocked | action.mask;
            if action.flags & SA_NODEFER == 0 {
                during |= bit(sig);
            }
            self.set_blocked(during);
            if action.flags & SA_RESETHAND != 0 {
                self.actions[sig as usize - 1] = Action::default();
            }
            return Some(Disposition::Handle { sig, action, restore_mask });
        }
    }
}
