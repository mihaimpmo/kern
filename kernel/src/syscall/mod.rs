//! System calls via the `syscall` instruction.
//!
//! ABI (same registers as Linux): number in rax, arguments in rdi, rsi, rdx,
//! r10, r8, r9; result in rax. Negative results are -errno (Linux values).
//!
//! Processes and time (process.rs):
//!    0 exit(code)                          6 uptime_ms()
//!    3 getpid()                            7 spawn(path, len, argv, argc) -> pid
//!    4 sleep_ms(ms)                        8 wait(pid, code: *mut i64)
//!    5 yield()                            10 sysinfo(buf: *mut [u64; 9])
//!   11 ps(buf, len) -> bytes              12 try_wait(code: *mut i64) -> pid, or 0
//!
//! Memory (memory.rs):
//!   13 brk(addr) -> new end of the heap (0: query; unchanged on failure)
//!   14 mmap(addr, len, prot, flags) -> addr      anonymous memory only
//!   15 munmap(addr, len)                  16 mprotect(addr, len, prot)
//!
//! Files (file.rs):
//!    1 write(fd, buf, len) -> bytes        2 read(fd, buf, len) -> bytes
//!   17 open(path, len, flags) -> fd       18 close(fd)
//!   19 lseek(fd, offset, whence) -> offset
//!   20 stat(path, len, *Stat)             21 fstat(fd, *Stat)
//!   34 rename(old, old_len, new, new_len) 35 statfs(path, len, *[u64; 5])
//!   22 readdir(fd, buf, len) -> bytes     (kind byte + name; 0 at the end)
//!   23 mkdir(path, len)                   24 unlink(path, len)
//!   25 chdir(path, len)                   26 getcwd(buf, len) -> bytes
//!   27 dup(fd, flags) -> fd               28 dup2(fd, new_fd) -> new_fd
//!   29 pipe(fds: *mut [u64; 2], flags)    (read end, write end)
//!
//! Descriptors opened with O_CLOEXEC (open, pipe, dup) aren't passed on to
//! spawned children; dup2 always makes an inheritable one.
//!
//! Signals and time (process.rs):
//!   30 kill(pid, signal)        pid > 0 a process, < 0 a group, 0 our group
//!   31 setpgid(pid, pgid)       32 set_foreground(pgid)   (Ctrl+C target)
//!   33 time_ms() -> milliseconds since 1970 (UTC)
//!   37 sigaction(sig, act: *const Action, old: *mut Action)   (signal.rs)
//!   38 sigprocmask(how, set: *const u64, old: *mut u64)
//!   39 sigreturn()               only from a handler's restorer
//!   40 sigsuspend(mask) -> EINTR, once a signal has been handled
//!
//! Programs (process.rs):
//!   36 exec(path, len, argv, argc)       replaces the program; no return
//!
//! Exit statuses (wait, try_wait) are the exit code in the low 32 bits, or
//! 1 << 32 | signal for a process that was killed. CPU faults in user mode
//! raise SIGSEGV, SIGILL, SIGFPE, ...; see `task::signal` for what signals do.
//!
//! (9 was `programs`, replaced by reading /bin.) Strings are (pointer,
//! length) pairs, never NUL-terminated; `argv` points to `argc` such pairs,
//! which the kernel passes on to the program in the usual C layout. PROT_*,
//! MAP_* and O_* flags have Linux's values.
//!
//! User memory is only ever touched through `copy_from_user` / `copy_to_user`,
//! which check the pages and open the SMAP window around the copy.

mod file;
mod memory;
mod process;
mod signal;

pub use signal::deliver_from_interrupt;

use crate::arch::cpu::{self, msr, rdmsr, wrmsr};
use crate::arch::gdt;
use crate::fs::FsError;
use crate::mm::uspace::{MemError, UserSpace};
use crate::mm::vmm;
use crate::task::{self, exec::LoadError, ExitStatus};
use alloc::{string::String, vec, vec::Vec};
use core::arch::global_asm;

/// An error number (positive; returned to user space negated).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Errno(i64);

pub const EPERM: Errno = Errno(1);
pub const ENOENT: Errno = Errno(2);
pub const ESRCH: Errno = Errno(3);
pub const EINTR: Errno = Errno(4);
pub const EIO: Errno = Errno(5);
pub const E2BIG: Errno = Errno(7);
pub const ENOEXEC: Errno = Errno(8);
pub const EBADF: Errno = Errno(9);
pub const ECHILD: Errno = Errno(10);
pub const ENOMEM: Errno = Errno(12);
pub const EFAULT: Errno = Errno(14);
pub const EEXIST: Errno = Errno(17);
pub const EXDEV: Errno = Errno(18);
pub const ENOTDIR: Errno = Errno(20);
pub const EPIPE: Errno = Errno(32);
pub const ERANGE: Errno = Errno(34);
pub const EISDIR: Errno = Errno(21);
pub const EINVAL: Errno = Errno(22);
pub const EMFILE: Errno = Errno(24);
pub const ENOSPC: Errno = Errno(28);
pub const ENAMETOOLONG: Errno = Errno(36);
pub const ENOSYS: Errno = Errno(38);
pub const ENOTEMPTY: Errno = Errno(39);

impl From<FsError> for Errno {
    fn from(e: FsError) -> Errno {
        match e {
            FsError::NotFound => ENOENT,
            FsError::Exists => EEXIST,
            FsError::NotDir => ENOTDIR,
            FsError::IsDir => EISDIR,
            FsError::NotEmpty => ENOTEMPTY,
            FsError::Invalid => EINVAL,
            FsError::NameTooLong => ENAMETOOLONG,
            FsError::BadFd => EBADF,
            FsError::TooManyFiles => EMFILE,
            FsError::NoMemory => ENOMEM,
            FsError::Interrupted => EINTR,
            FsError::BrokenPipe => EPIPE,
            FsError::NoSpace => ENOSPC,
            FsError::Io => EIO,
            FsError::CrossDevice => EXDEV,
        }
    }
}

impl From<MemError> for Errno {
    fn from(e: MemError) -> Errno {
        match e {
            MemError::NoMemory => ENOMEM,
            MemError::Invalid => EINVAL,
            MemError::Fault => EFAULT,
        }
    }
}

impl From<LoadError> for Errno {
    fn from(e: LoadError) -> Errno {
        match e {
            LoadError::NoMemory => ENOMEM,
            LoadError::ArgsTooLong => E2BIG,
            LoadError::BadElf(_) => ENOEXEC,
        }
    }
}

type SysResult = Result<u64, Errno>;

const SYS_SIGRETURN: u64 = 39;

// The frame starts like an interrupt's (ss, rsp, rflags, cs, rip), so it can
// return either way: `sysretq` normally, which takes rip and rflags from rcx
// and r11, or `iretq` when every register must come back as saved (after
// `sigreturn`, which resumes code interrupted at any instruction).
global_asm!(
    r#"
.section .text
.global syscall_entry
syscall_entry:
    swapgs                         // GS -> per-CPU data
    mov gs:[8], rsp                // stash user rsp
    mov rsp, gs:[0]                // kernel stack of this task
    push 0x1b                      // user ss
    push qword ptr gs:[8]          // user rsp
    push r11                       // user rflags
    push 0x23                      // user cs
    push rcx                       // user rip
    push rax
    push rcx
    push r11
    push rdi
    push rsi
    push rdx
    push r10
    push r8
    push r9
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp
    call syscall_dispatch
    cli
    test al, al                    // pops leave the flags alone
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    pop r9
    pop r8
    pop r10
    pop rdx
    pop rsi
    pop rdi
    pop r11
    pop rcx
    pop rax
    jnz 1f
    pop rcx                        // rip
    add rsp, 8                     // cs
    pop r11                        // rflags
    pop rsp                        // back on the user stack
    swapgs
    sysretq
1:
    swapgs
    iretq
"#
);

extern "C" {
    fn syscall_entry();
}

/// What `syscall_entry` pushes, lowest address first.
#[repr(C)]
pub struct SyscallFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbp: u64,
    rbx: u64,
    r9: u64,
    r8: u64,
    r10: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    r11: u64,
    rcx: u64,
    rax: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// Per CPU: the syscall MSRs are per-CPU registers.
pub fn init() {
    unsafe {
        wrmsr(msr::EFER, rdmsr(msr::EFER) | 1); // SCE: enable syscall/sysret
        // syscall: CS = 0x08, SS = 0x10.
        // sysret:  CS = base + 16, SS = base + 8. The base must already carry
        // RPL 3: Intel forces RPL 3 onto SS, but AMD copies the selector as is,
        // which would leave user mode with SS = 0x18 and make the next iretq
        // back to user mode fault with #GP(0x18).
        let sysret_base = gdt::KERNEL_DS as u64 | 3; // 0x13 -> SS 0x1b, CS 0x23
        let star = (sysret_base << 48) | ((gdt::KERNEL_CS as u64) << 32);
        wrmsr(msr::STAR, star);
        wrmsr(msr::LSTAR, syscall_entry as *const () as u64);
        // Clear IF, TF, DF, AC on entry.
        wrmsr(msr::FMASK, 0x200 | 0x100 | 0x400 | 0x40000);
    }
}

/// Returns true to go back to user mode through `iretq` (every register as
/// in the frame) rather than `sysretq` (rcx and r11 clobbered).
#[no_mangle]
extern "C" fn syscall_dispatch(f: &mut SyscallFrame) -> bool {
    cpu::sti();
    let iret = f.rax == SYS_SIGRETURN;
    if iret {
        signal::sigreturn(f);
    } else {
        f.rax = match call(f) {
            Ok(value) => value,
            Err(Errno(e)) => (-e) as u64,
        };
    }

    signal::deliver_from_syscall(f);
    // Returning to a non-canonical address would fault in ring 0. Handlers
    // and sigreturn are checked already; this catches the rest.
    if f.rip >= vmm::USER_END {
        task::exit_current(ExitStatus::Killed(task::signal::SIGSEGV));
    }
    f.rflags = (f.rflags & !0x3000) | 0x200; // IOPL 0, interrupts on
    iret
}

fn call(f: &mut SyscallFrame) -> SysResult {
    let (a0, a1, a2, a3) = (f.rdi, f.rsi, f.rdx, f.r10);
    match f.rax {
        0 => task::exit_current(ExitStatus::Exited(a0 as i32)),
        1 => file::write(a0, a1, a2),
        2 => file::read(a0, a1, a2),
        3 => Ok(task::current_pid()),
        4 => {
            task::sleep_ms(a0);
            Ok(0)
        }
        5 => {
            task::yield_now();
            Ok(0)
        }
        6 => Ok(task::uptime_ms()),
        7 => process::spawn(a0, a1, a2, a3),
        8 => process::wait(a0, a1),
        10 => process::sysinfo(a0),
        11 => process::ps(a0, a1),
        12 => process::try_wait(a0),
        13 => memory::brk(a0),
        14 => memory::mmap(a0, a1, a2, a3),
        15 => memory::munmap(a0, a1),
        16 => memory::mprotect(a0, a1, a2),
        17 => file::open(a0, a1, a2),
        18 => file::close(a0),
        19 => file::lseek(a0, a1 as i64, a2),
        20 => file::stat(a0, a1, a2),
        21 => file::fstat(a0, a1),
        22 => file::readdir(a0, a1, a2),
        23 => file::mkdir(a0, a1),
        24 => file::unlink(a0, a1),
        25 => file::chdir(a0, a1),
        26 => file::getcwd(a0, a1),
        27 => file::dup(a0, a1),
        28 => file::dup2(a0, a1),
        29 => file::pipe(a0, a1),
        30 => process::kill(a0, a1),
        31 => process::setpgid(a0, a1),
        32 => process::set_foreground(a0),
        33 => process::time_ms(),
        34 => file::rename(a0, a1, a2, a3),
        35 => file::statfs(a0, a1, a2),
        36 => process::exec(f, a0, a1, a2, a3),
        37 => signal::sigaction(a0, a1, a2),
        38 => signal::sigprocmask(a0, a1, a2),
        40 => signal::sigsuspend(a0),
        _ => Err(ENOSYS),
    }
}

// ------------------------------------------------------------ user memory

/// Run `f` on the calling process's memory, locked (so interrupts are off).
fn with_user_space<R>(f: impl FnOnce(&mut UserSpace) -> R) -> Result<R, Errno> {
    // Only user processes make system calls.
    let process = task::current_process().ok_or(EFAULT)?;
    let mut space = process.space.lock();
    Ok(f(&mut space))
}

/// Check that [ptr, ptr + len) is memory the process may read (and write, if
/// asked), mapping pages it hasn't touched yet. An empty range is always fine.
fn check_user(ptr: u64, len: u64, write: bool) -> Result<(), Errno> {
    if len == 0 {
        return Ok(());
    }
    with_user_space(|s| s.populate(ptr, len, write))?.map_err(|_| EFAULT)
}

/// Fill `out` from user memory at `src`, all or nothing.
fn copy_from_user(out: &mut [u8], src: u64) -> Result<(), Errno> {
    if out.is_empty() {
        return Ok(());
    }
    // The address-space lock keeps interrupts off across check and copy:
    // nothing can unmap the range in between, and RFLAGS.AC can't leak into
    // another task.
    with_user_space(|s| {
        s.populate(src, out.len() as u64, false).map_err(|_| EFAULT)?;
        cpu::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(src as *const u8, out.as_mut_ptr(), out.len()) };
        cpu::user_access_end();
        Ok(())
    })?
}

/// Copy `data` to user memory at `dst`, all or nothing.
fn copy_to_user(dst: u64, data: &[u8]) -> Result<(), Errno> {
    if data.is_empty() {
        return Ok(());
    }
    with_user_space(|s| {
        s.populate(dst, data.len() as u64, true).map_err(|_| EFAULT)?;
        cpu::user_access_begin();
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, data.len()) };
        cpu::user_access_end();
        Ok(())
    })?
}

/// Copy as much of `data` as fits in the user buffer; returns bytes copied.
fn copy_out(data: &[u8], buf: u64, len: u64) -> SysResult {
    let n = (data.len() as u64).min(len) as usize;
    copy_to_user(buf, &data[..n])?;
    Ok(n as u64)
}

/// A user string given as (ptr, len), at most `max` bytes (else `too_long`).
fn user_bytes(ptr: u64, len: u64, max: usize, too_long: Errno) -> Result<Vec<u8>, Errno> {
    if len > max as u64 {
        return Err(too_long);
    }
    let mut buf = vec![0u8; len as usize];
    copy_from_user(&mut buf, ptr)?;
    Ok(buf)
}

/// A path argument.
fn user_path(ptr: u64, len: u64) -> Result<String, Errno> {
    let bytes = user_bytes(ptr, len, crate::fs::MAX_PATH, ENAMETOOLONG)?;
    String::from_utf8(bytes).map_err(|_| EINVAL)
}
