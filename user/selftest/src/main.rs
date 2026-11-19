//! selftest: checks the kernel from user space. Prints one TAP-style line per
//! check and exits with the number of failures, so `make test` can boot with
//! `init=selftest qemu-exit` and read the result from QEMU's exit status.
//!
//! It also starts copies of itself in helper modes (`selftest exit 3`, ...)
//! to get child processes that behave in a known way.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use ulib::signal::{Handler, Regs, SIGCHLD, SIGILL, SIGUSR1, SIGUSR2};
use ulib::fs::{self, flags, Kind, Whence};
use ulib::{println, prot, signal, sys, syscall6, Errno, ExitStatus};

const NAME: &str = "selftest";
/// How a process that touched memory it may not ends.
const KILLED_SEGV: ExitStatus = ExitStatus::Killed(signal::SIGSEGV);
/// Mapped nowhere in user space, and not canonical-adjacent.
const BAD_PTR: u64 = 0xdead_0000_0000;

#[no_mangle]
fn main() -> i32 {
    let mut args = [""; 8];
    let mut argc = 0;
    for (slot, arg) in args.iter_mut().zip(ulib::args()) {
        *slot = arg;
        argc += 1;
    }
    if let Some(code) = signal_mode(&args[1..argc]) {
        return code;
    }
    match &args[1..argc] {
        [] => run_all(),
        ["exit", code] => code.parse().unwrap_or(-1),
        ["argcheck", rest @ ..] => (rest != ["one", "two words", ""]) as i32,
        ["fault"] => unsafe {
            core::ptr::write_volatile(0xffff_ffff_8000_0000 as *mut u64, 0);
            0
        },
        ["spin", ms] => {
            let end = ulib::uptime_ms() + ms.parse::<u64>().unwrap_or(0);
            while ulib::uptime_ms() < end {
                core::hint::spin_loop();
            }
            0
        }
        ["use-after-munmap"] => unsafe {
            let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap();
            ulib::munmap(p, 4096).unwrap();
            core::ptr::read_volatile(p) as i32
        },
        ["write-readonly"] => unsafe {
            let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap();
            ulib::mprotect(p, 4096, prot::READ).unwrap();
            let _ = core::ptr::read_volatile(p); // reading is still fine
            core::ptr::write_volatile(p, 1);
            0
        },
        ["exec-heap"] => unsafe {
            // A `ret` instruction in writable memory must not be executable (NX).
            let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap();
            p.write(0xc3);
            let f: extern "C" fn() = core::mem::transmute(p);
            f();
            0
        },
        ["stack", mib] => {
            // About `mib` MiB of stack: fine below 8 MiB, a fault above.
            let depth = mib.parse::<u32>().unwrap_or(0) * 256;
            (recurse(depth) != 0) as i32 // recurse returns 0 when it gets back up
        }
        ["hog", mib] => {
            let mut v = vec![0u8; mib.parse::<usize>().unwrap_or(0) << 20];
            for i in (0..v.len()).step_by(4096) {
                v[i] = 1;
            }
            (black_box(&v).iter().map(|&b| b as usize).sum::<usize>() != v.len() / 4096) as i32
        }
        ["float", scale] => float_work(scale.parse().unwrap_or(1)),
        ["mxcsr"] => {
            // Round toward zero; it must survive other processes using the
            // default rounding mode on the same CPUs.
            let set: u32 = 0x7f80;
            unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &set) };
            let end = ulib::uptime_ms() + 300;
            while ulib::uptime_ms() < end {
                ulib::sleep_ms(1);
                for _ in 0..10_000 {
                    core::hint::spin_loop();
                }
            }
            let mut now: u32 = 0;
            unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut now) };
            (now != set) as i32
        }
        ["set-ac", ms] => {
            // RFLAGS.AC set in user mode while timer interrupts come in:
            // the kernel must not run its handlers with it (SMAP off).
            unsafe { core::arch::asm!("pushfq", "or qword ptr [rsp], 0x40000", "popfq") };
            let end = ulib::uptime_ms() + ms.parse::<u64>().unwrap_or(0);
            while ulib::uptime_ms() < end {
                core::hint::spin_loop();
            }
            0
        }
        ["say", text] => {
            println!("{}", text);
            0
        }
        // Exit code: how many bytes standard input had.
        ["stdin-len"] => fs::read_to_end(ulib::STDIN).map_or(-1, |d| d.len() as i32),
        ["write-bytes", n] => {
            let chunk = [b'x'; 4096];
            let mut left = n.parse::<usize>().unwrap_or(0);
            while left > 0 {
                let k = left.min(chunk.len());
                if ulib::write_all(ulib::STDOUT, &chunk[..k]).is_err() {
                    return 1;
                }
                left -= k;
            }
            0
        }
        // Exit code 1 if descriptor `fd` is open, 0 if not.
        ["fd-open", fd] => fs::fstat(fd.parse().unwrap_or(999)).is_ok() as i32,
        ["spin-forever"] => loop {
            core::hint::spin_loop();
        },
        ["sleep-forever"] => {
            ulib::sleep_ms(3_600_000);
            0
        }
        ["read-forever"] => fs::read_to_end(ulib::STDIN).map_or(1, |_| 0),
        ["illegal"] => unsafe {
            core::arch::asm!("ud2");
            0
        },
        ["divide"] => unsafe {
            // Hardware divide by zero (Rust's own `/` would panic first).
            core::arch::asm!("xor edx, edx", "xor ecx, ecx", "div ecx", out("eax") _, out("edx") _, out("ecx") _);
            0
        },
        ["orphan"] => {
            // Our child finishes (and is cleaned up) while we are alive; then
            // we exit without waiting for it.
            let _ = ulib::spawn(NAME, &["exit", "0"]);
            ulib::sleep_ms(100);
            0
        }
        _ => {
            println!("usage: selftest");
            2
        }
    }
}

struct Suite {
    run: u32,
    failed: u32,
}

impl Suite {
    fn check(&mut self, name: &str, ok: bool, detail: core::fmt::Arguments) {
        self.run += 1;
        if ok {
            println!("ok {} - {}", self.run, name);
        } else {
            self.failed += 1;
            println!("\x1b[91mnot ok {} - {}: {}\x1b[0m", self.run, name, detail);
        }
    }
}

macro_rules! check {
    ($suite:expr, $name:expr, $ok:expr) => {
        $suite.check($name, $ok, format_args!("{}", stringify!($ok)))
    };
    ($suite:expr, $name:expr, $ok:expr, $($detail:tt)+) => {
        $suite.check($name, $ok, format_args!($($detail)+))
    };
}

fn raw(n: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    unsafe { syscall6(n, a0, a1, a2, 0, 0, 0) }
}

/// Run `selftest <args>` and wait for its exit code.
fn exited(code: i32) -> Result<ExitStatus, Errno> {
    Ok(ExitStatus::Exited(code))
}

fn run_child(args: &[&str]) -> Result<ExitStatus, Errno> {
    ulib::wait(ulib::spawn(NAME, args)?)
}

fn run_all() -> i32 {
    println!("selftest: starting (pid {})", ulib::getpid());
    let mut t = Suite { run: 0, failed: 0 };
    syscall_errors(&mut t);
    processes(&mut t);
    timing(&mut t);
    multicore(&mut t);
    memory(&mut t);
    files(&mut t);
    pipes(&mut t);
    signals(&mut t);
    exec(&mut t);
    handlers(&mut t);
    leaks(&mut t);
    println!("selftest: {} checks, {} failed", t.run, t.failed);
    t.failed as i32
}

fn syscall_errors(t: &mut Suite) {
    let r = raw(999, 0, 0, 0);
    check!(t, "unknown syscall is ENOSYS", r == -Errno::ENOSYS.0, "got {}", r);
    let r = ulib::write(ulib::STDOUT, &[]);
    check!(t, "zero-length write", r == Ok(0), "got {:?}", r);
    let r = raw(sys::WRITE, ulib::STDOUT, 0, 0);
    check!(t, "zero-length write from NULL", r == 0, "got {}", r);
    let r = raw(sys::WRITE, ulib::STDOUT, BAD_PTR, 4);
    check!(t, "write from bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = raw(sys::WRITE, 7, BAD_PTR, 4);
    check!(t, "write to bad fd is EBADF", r == -Errno::EBADF.0, "got {}", r);
    let r = raw(sys::READ, ulib::STDIN, BAD_PTR, 4);
    check!(t, "read into bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = raw(sys::SYSINFO, BAD_PTR, 0, 0);
    check!(t, "sysinfo into bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    // The kernel's own code is mapped, but not for user mode.
    let r = raw(sys::WRITE, ulib::STDOUT, 0xffff_ffff_8000_0000, 4);
    check!(t, "write from kernel memory is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = raw(sys::WAIT, 1, BAD_PTR, 0);
    check!(t, "wait into bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let r = ulib::wait(1);
    check!(t, "wait for a non-child is ECHILD", r == Err(Errno::ECHILD), "got {:?}", r);
    check!(t, "try_wait without children", ulib::try_wait().is_none());
}

fn processes(t: &mut Suite) {
    let r = ulib::spawn("no-such-program", &[]);
    check!(t, "spawn of a missing program is ENOENT", r == Err(Errno::ENOENT), "got {:?}", r);
    let long = [b'x'; 300];
    let r = ulib::spawn(core::str::from_utf8(&long).unwrap(), &[]);
    check!(t, "spawn with a 300-byte name is ENAMETOOLONG", r == Err(Errno::ENAMETOOLONG), "got {:?}", r);
    // Raw call: ulib::spawn would refuse this before asking the kernel.
    let pairs = [[0u64; 2]; ulib::MAX_ARGS + 1];
    let r = unsafe {
        let name = NAME.as_ptr() as u64;
        syscall6(sys::SPAWN, name, NAME.len() as u64, pairs.as_ptr() as u64, pairs.len() as u64, 0, 0)
    };
    check!(t, "spawn with too many arguments is E2BIG", r == -Errno::E2BIG.0, "got {}", r);

    let r = run_child(&["exit", "3"]);
    check!(t, "child exit code", r == exited(3), "got {:?}", r);
    let r = run_child(&["exit", "-5"]);
    check!(t, "negative exit code is not an error", r == exited(-5), "got {:?}", r);
    let r = run_child(&["argcheck", "one", "two words", ""]);
    check!(t, "arguments reach the child intact", r == exited(0), "got {:?}", r);
    let r = run_child(&["fault"]);
    check!(t, "faulting child is killed", r == Ok(KILLED_SEGV), "got {:?}", r);

    let pid = ulib::spawn(NAME, &["exit", "9"]);
    ulib::sleep_ms(50);
    let r = ulib::try_wait();
    check!(t, "try_wait collects a finished child", r.is_some() && r == pid.ok().map(|p| (p, ExitStatus::Exited(9))), "got {:?}", r);

    // Several children at once, collected in reverse order.
    let mut pids = [0; 16];
    for (i, pid) in pids.iter_mut().enumerate() {
        *pid = ulib::spawn(NAME, &["exit", ["0", "1", "2", "3"][i % 4]]).unwrap_or(0);
    }
    let mut all_ok = pids.iter().all(|&p| p != 0);
    for (i, &pid) in pids.iter().enumerate().rev() {
        all_ok &= ulib::wait(pid) == exited((i % 4) as i32);
    }
    check!(t, "16 concurrent children", all_ok);
}

fn timing(t: &mut Suite) {
    let start = ulib::uptime_ms();
    ulib::sleep_ms(50);
    let slept = ulib::uptime_ms() - start;
    check!(t, "sleep_ms(50) sleeps about 50 ms", (50..200).contains(&slept), "slept {} ms", slept);

    // A busy child must not keep us from waking up: preemption.
    let spinner = ulib::spawn(NAME, &["spin", "300"]);
    let start = ulib::uptime_ms();
    ulib::sleep_ms(20);
    let late = ulib::uptime_ms() - start;
    check!(t, "busy process does not starve others", late < 100, "woke after {} ms", late);
    if let Ok(pid) = spinner {
        let _ = ulib::wait(pid);
    }
}

fn multicore(t: &mut Suite) {
    let cpus = ulib::sysinfo().cpus;
    check!(t, "sysinfo reports CPUs", cpus >= 1, "cpus = {}", cpus);

    // Many short-lived processes at once: races in spawn/exit/reap show up
    // here, especially with several CPUs.
    let mut all_ok = true;
    for round in 0..8 {
        let mut pids = [0; 24];
        for pid in pids.iter_mut() {
            *pid = ulib::spawn(NAME, &["exit", "7"]).unwrap_or(0);
        }
        for &pid in &pids {
            let r = ulib::wait(pid);
            if pid == 0 || r != exited(7) {
                println!("  round {}: pid {} -> {:?}", round, pid, r);
                all_ok = false;
            }
        }
    }
    check!(t, "spawn storm: 8 x 24 concurrent children", all_ok);

    // Floating point in several processes at once (register state is saved
    // per task), plus one with a non-default SSE control register.
    let mut pids = [0; 5];
    for (i, pid) in pids.iter_mut().enumerate() {
        let r = if i == 4 { ulib::spawn(NAME, &["mxcsr"]) } else { ulib::spawn(NAME, &["float", ["1", "2", "3", "5"][i]]) };
        *pid = r.unwrap_or(0);
    }
    let codes = pids.map(|pid| ulib::wait(pid));
    check!(t, "SSE state is per process", codes.iter().all(|c| *c == exited(0)), "exit codes {:?}", codes);

    if cpus < 2 {
        return;
    }
    // N busy children of 300 ms each finish in about 300 ms with N CPUs,
    // N x 300 ms on one. Accept anything clearly better than serial.
    let n = cpus.min(4);
    let start = ulib::uptime_ms();
    let mut pids = [0; 4];
    for pid in pids[..n as usize].iter_mut() {
        *pid = ulib::spawn(NAME, &["spin", "300"]).unwrap_or(0);
    }
    for &pid in &pids[..n as usize] {
        let _ = ulib::wait(pid);
    }
    let took = ulib::uptime_ms() - start;
    let limit = 150 * (n + 1);
    println!("# {} x 300 ms of busy work took {} ms on {} CPUs", n, took, cpus);
    check!(t, "busy processes run in parallel", took < limit, "{} x 300 ms took {} ms (limit {})", n, took, limit);
}

/// Busy floating-point work for ~300 ms, preempted many times. Every partial
/// sum is an integer below 2^53, so the result is exact and known: if any
/// switch lost or mixed up SSE registers, it comes out wrong.
fn float_work(scale: u64) -> i32 {
    let factor = scale as f64;
    let mut acc = 0.0f64;
    let mut n: u64 = 0;
    let end = ulib::uptime_ms() + 300;
    while ulib::uptime_ms() < end {
        for _ in 0..100_000 {
            acc += black_box((n % 1000) as f64) * factor;
            n += 1;
        }
    }
    let full = n / 1000;
    let expected = scale * (full * 499_500 + (0..n % 1000).sum::<u64>());
    if acc == expected as f64 {
        0
    } else {
        println!("  float {}: {} iterations gave {}, expected {}", scale, n, acc, expected);
        1
    }
}

/// About 4 KiB of stack per level.
fn recurse(depth: u32) -> u64 {
    let buf = black_box([depth as u8; 4000]);
    if depth == 0 {
        buf[0] as u64
    } else {
        recurse(depth - 1) + buf[1] as u64 - buf[2] as u64
    }
}

fn free_memory() -> u64 {
    ulib::sysinfo().free_memory
}

fn memory(t: &mut Suite) {
    // The heap, through the global allocator.
    let v: Vec<u32> = (0..200_000).collect();
    let sum: u64 = v.iter().map(|&x| x as u64).sum();
    check!(t, "Vec of 200k elements", sum == 199_999 * 200_000 / 2, "sum = {}", sum);
    let mut s = String::new();
    for i in 0..1000 {
        s.push_str(if i % 2 == 0 { "ab" } else { "c" });
    }
    check!(t, "String growth", s.len() == 1500 && s.ends_with("abc"), "len = {}", s.len());
    let big = vec![0u8; 16 << 20];
    check!(t, "16 MiB zeroed Box", big.iter().all(|&b| b == 0));
    drop(big);
    let boxes: Vec<Box<[u8; 100]>> = (0..1000).map(|i| Box::new([i as u8; 100])).collect();
    check!(t, "many small boxes", boxes.iter().enumerate().all(|(i, b)| b[99] == i as u8));
    drop(boxes);

    // brk: grow, use, shrink; regrown pages come back zeroed.
    let base = ulib::brk(0);
    let grown = ulib::brk(base + (64 << 10));
    unsafe { core::ptr::write_volatile((base + 4096) as *mut u8, 0x5a) };
    let shrunk = ulib::brk(base);
    let regrown = ulib::brk(base + (64 << 10));
    let zero = unsafe { core::ptr::read_volatile((base + 4096) as *const u8) };
    ulib::brk(base);
    check!(t, "brk grows and shrinks", grown == base + (64 << 10) && shrunk == base && regrown == grown);
    check!(t, "regrown brk memory is zeroed", zero == 0, "byte = {:#x}", zero);
    check!(t, "brk below the heap is refused", ulib::brk(4096) == base);

    // mmap / munmap.
    match ulib::mmap(4 << 20, prot::READ | prot::WRITE) {
        Ok(p) => unsafe {
            for page in 0..1024 {
                p.add(page * 4096).write_volatile(page as u8);
            }
            let ok = (0..1024).all(|page| p.add(page * 4096).read_volatile() == page as u8);
            check!(t, "4 MiB mmap is usable", ok);
            check!(t, "munmap", ulib::munmap(p, 4 << 20).is_ok());
        },
        Err(e) => check!(t, "4 MiB mmap is usable", false, "{:?}", e),
    }

    // Pages are only allocated when touched, and munmap gives them back.
    let before = free_memory();
    let reserved = ulib::mmap(1 << 30, prot::READ | prot::WRITE);
    let after_reserve = free_memory();
    let after_touch = match reserved {
        Ok(p) => unsafe {
            p.add(512 << 20).write_volatile(1);
            free_memory()
        },
        Err(_) => 0,
    };
    if let Ok(p) = reserved {
        let _ = unsafe { ulib::munmap(p, 1 << 30) };
    }
    let after_unmap = free_memory();
    check!(t, "reserving 1 GiB costs (almost) nothing", reserved.is_ok() && before - after_reserve < (64 << 10),
           "{:?}, {} bytes", reserved, before - after_reserve);
    check!(t, "touching a page allocates it", after_reserve - after_touch >= 4096,
           "{} bytes", after_reserve - after_touch);
    check!(t, "munmap frees touched pages", after_unmap + (64 << 10) > before,
           "{} -> {} bytes free", before, after_unmap);

    // The kernel can write into memory the process hasn't touched yet.
    let p = ulib::mmap(4096, prot::READ | prot::WRITE).unwrap_or(core::ptr::null_mut());
    let r = raw(sys::SYSINFO, p as u64, 0, 0);
    check!(t, "syscall output into untouched memory", r == 0 && unsafe { *(p as *const u64) } > 0, "got {}", r);

    // mprotect keeps contents.
    unsafe { p.write_volatile(42) };
    let r1 = ulib::mprotect(p, 4096, prot::NONE);
    let r2 = raw(sys::WRITE, ulib::STDOUT, p as u64, 1);
    let r3 = ulib::mprotect(p, 4096, prot::READ | prot::WRITE);
    let kept = unsafe { p.read_volatile() };
    check!(t, "kernel can't read PROT_NONE memory", r1.is_ok() && r2 == -Errno::EFAULT.0, "{:?} {}", r1, r2);
    check!(t, "mprotect keeps contents", r3.is_ok() && kept == 42, "{:?} {}", r3, kept);
    let _ = unsafe { ulib::munmap(p, 4096) };

    // Bad arguments.
    let r = ulib::mmap(0, prot::READ);
    check!(t, "mmap of 0 bytes is EINVAL", r == Err(Errno::EINVAL), "got {:?}", r);
    let r = unsafe { syscall6(sys::MMAP, 0, 4096, prot::READ, 0x02, 0, 0) }; // not MAP_ANONYMOUS
    check!(t, "file-backed mmap is EINVAL", r == -Errno::EINVAL.0, "got {}", r);
    let r = ulib::mmap(4096, 8);
    check!(t, "unknown prot bits are EINVAL", r == Err(Errno::EINVAL), "got {:?}", r);
    let r = raw(sys::MUNMAP, 0x1000_0000_0123, 4096, 0);
    check!(t, "unaligned munmap is EINVAL", r == -Errno::EINVAL.0, "got {}", r);
    let r = ulib::mprotect(0x1000_0000_0000 as *mut u8, 4096, prot::READ);
    check!(t, "mprotect of unmapped memory is ENOMEM", r == Err(Errno::ENOMEM), "got {:?}", r);
    let fixed = ulib::mmap_fixed(0x2000_0000_0000 as *mut u8, 8192, prot::READ | prot::WRITE);
    check!(t, "MAP_FIXED maps at the address", fixed == Ok(0x2000_0000_0000 as *mut u8), "got {:?}", fixed);
    if let Ok(p) = fixed {
        let _ = unsafe { ulib::munmap(p, 8192) };
    }

    // Faults that must kill, and stack use that must not.
    for mode in ["use-after-munmap", "write-readonly", "exec-heap"] {
        let r = run_child(&[mode]);
        check!(t, mode, r == Ok(KILLED_SEGV), "got {:?}", r);
    }
    let r = run_child(&["stack", "6"]);
    check!(t, "6 MiB of stack works", r == exited(0), "got {:?}", r);
    let r = run_child(&["stack", "9"]);
    check!(t, "stack overflow is a fault", r == Ok(KILLED_SEGV), "got {:?}", r);
}

fn write_file(path: &str, data: &[u8]) -> Result<(), Errno> {
    let fd = fs::open(path, flags::WRITE_ONLY | flags::CREATE | flags::TRUNCATE)?;
    let r = ulib::write_all(fd, data);
    let _ = fs::close(fd);
    r
}

/// Run `selftest <args>` with fd `target` pointed at `path`.
fn run_redirected(target: fs::Fd, path: &str, mode: u64, args: &[&str]) -> Result<ExitStatus, Errno> {
    let file = fs::open(path, mode)?;
    let saved = fs::dup(target, flags::CLOSE_ON_SPAWN)?;
    fs::dup2(file, target)?;
    let _ = fs::close(file);
    let pid = ulib::spawn(NAME, args);
    let _ = fs::dup2(saved, target);
    let _ = fs::close(saved);
    ulib::wait(pid?)
}

fn files(t: &mut Suite) {
    // Programs come from the initramfs.
    let s = fs::stat("/bin/selftest");
    check!(t, "/bin/selftest is a file", matches!(s, Ok(fs::Stat { kind: Kind::File, size, .. }) if size > 0), "{:?}", s);
    let head = fs::read_file("/bin/selftest").map(|d| d.get(..4) == Some(b"\x7fELF"));
    check!(t, "reading a program file", head == Ok(true), "{:?}", head);
    let r = ulib::spawn("/bin/selftest", &["exit", "4"]).and_then(ulib::wait);
    check!(t, "spawn by absolute path", r == exited(4), "{:?}", r);
    let devices = fs::read_dir("/dev").map(|d| d.flatten().filter(|e| e.kind == Kind::Device).count());
    check!(t, "/dev has the console and null", devices == Ok(2), "{:?}", devices);
    let null = fs::open("/dev/null", flags::READ_WRITE);
    let mut b = [0u8; 4];
    let ok = null.map_or(false, |fd| ulib::write(fd, b"gone") == Ok(4) && ulib::read(fd, &mut b) == Ok(0));
    check!(t, "/dev/null", ok);
    if let Ok(fd) = null {
        let _ = fs::close(fd);
    }
    let mut buf = [0u8; 64];
    let _ = fs::chdir("/");
    let _ = fs::chdir("..");
    check!(t, ".. at / stays at /", fs::getcwd(&mut buf) == Ok("/"));
    let long = [b'x'; 300];
    let r = fs::stat(core::str::from_utf8(&long).unwrap());
    check!(t, "a 300-byte name is ENAMETOOLONG", r == Err(Errno::ENAMETOOLONG), "{:?}", r);

    // The same behaviour from the in-memory filesystem and from the disk.
    file_suite(t, "/tmp");
    match fs::stat("/disk") {
        Ok(s) if s.kind == Kind::Dir => {
            file_suite(t, "/disk");
            disk(t);
        }
        _ => println!("# no disk at /disk: skipping the disk checks"),
    }
}

/// Generic file and directory behaviour, in `base`/st.
fn file_suite(t: &mut Suite, base: &str) {
    let dir = alloc::format!("{}/st", base);
    let path = |name: &str| alloc::format!("{}/{}", dir, name);
    let name = |what: &str| alloc::format!("{}: {}", base, what);
    let a = path("a.txt");

    // Create, write, read back.
    let _ = fs::mkdir(&dir);
    check!(t, &name("mkdir twice is EEXIST"), fs::mkdir(&dir) == Err(Errno::EEXIST));
    let r = write_file(&a, b"hello world");
    check!(t, &name("create and write a file"), r.is_ok(), "{:?}", r);
    let s = fs::stat(&a);
    check!(t, &name("stat reports the size"), s.map(|s| s.size) == Ok(11), "{:?}", s);
    let d = fs::read_file(&a);
    check!(t, &name("read the file back"), d.as_deref() == Ok(&b"hello world"[..]), "{:?}", d);

    // Offsets, appending, truncation, exclusive creation.
    if let Ok(fd) = fs::open(&a, flags::READ_WRITE) {
        let mut buf = [0u8; 5];
        let r = fs::seek(fd, 6, Whence::Start).and_then(|_| ulib::read(fd, &mut buf));
        check!(t, &name("seek then read"), r == Ok(5) && &buf == b"world", "{:?}", r);
        let end = fs::seek(fd, 0, Whence::End);
        check!(t, &name("seek to the end"), end == Ok(11), "{:?}", end);
        let dup = fs::dup(fd, 0);
        let _ = fs::seek(fd, 0, Whence::Start);
        let mut x = [0u8; 3];
        let mut y = [0u8; 3];
        let _ = ulib::read(fd, &mut x);
        let _ = dup.and_then(|d| ulib::read(d, &mut y));
        check!(t, &name("dup shares the offset"), &x == b"hel" && &y == b"lo ", "{:?} {:?}", x, y);
        if let Ok(d) = dup {
            let _ = fs::close(d);
        }
        let _ = fs::close(fd);
    }
    if let Ok(fd) = fs::open(&a, flags::WRITE_ONLY | flags::APPEND) {
        let _ = fs::seek(fd, 0, Whence::Start); // ignored: appends go to the end
        let _ = ulib::write_all(fd, b"!");
        let _ = fs::close(fd);
    }
    let d = fs::read_file(&a);
    check!(t, &name("O_APPEND writes at the end"), d.as_deref() == Ok(&b"hello world!"[..]), "{:?}", d);
    let r = fs::open(&a, flags::WRITE_ONLY | flags::CREATE | flags::EXCLUSIVE);
    check!(t, &name("O_EXCL on an existing file is EEXIST"), r == Err(Errno::EEXIST), "{:?}", r);
    if let Ok(fd) = fs::open(&a, flags::WRITE_ONLY | flags::TRUNCATE) {
        let _ = fs::close(fd);
    }
    check!(t, &name("O_TRUNC empties the file"), fs::stat(&a).map(|s| s.size) == Ok(0));
    let _ = write_file(&a, b"abc");
    let s = fs::stat(&a);
    check!(t, &name("stat reports a modification time"), s.map_or(false, |s| s.mtime > 1_577_836_800), "{:?}", s);

    // Directories.
    let _ = fs::mkdir(&path("d"));
    let names: Vec<(String, Kind)> = fs::read_dir(&dir)
        .map(|dir| dir.flatten().map(|e| (String::from(e.name()), e.kind)).collect())
        .unwrap_or_default();
    let want = [(String::from("a.txt"), Kind::File), (String::from("d"), Kind::Dir)];
    check!(t, &name("readdir lists entries in order"), names == want, "{:?}", names);

    // Relative paths and the working directory.
    let mut buf = [0u8; 64];
    let r = fs::chdir(&dir);
    let cwd = fs::getcwd(&mut buf).map(String::from);
    check!(t, &name("chdir and getcwd"), r.is_ok() && cwd.as_deref() == Ok(&dir[..]), "{:?} {:?}", r, cwd);
    check!(t, &name("relative path"), fs::stat("a.txt").map(|s| s.size) == Ok(3));
    check!(t, &name("path with . and .."), fs::stat("../st/./d/../a.txt").map(|s| s.size) == Ok(3));
    let r = ulib::spawn("../../bin/selftest", &["exit", "6"]).and_then(ulib::wait);
    check!(t, &name("spawn by relative path"), r == exited(6), "{:?}", r);
    check!(t, &name("chdir to a file is ENOTDIR"), fs::chdir("a.txt") == Err(Errno::ENOTDIR));
    let _ = fs::chdir("/");

    // Errors.
    check!(t, &name("open of a missing file is ENOENT"), fs::open(&path("nope"), flags::READ_ONLY) == Err(Errno::ENOENT));
    check!(t, &name("writing a directory is EISDIR"), fs::open(&dir, flags::WRITE_ONLY) == Err(Errno::EISDIR));
    let r = fs::open(&a, flags::READ_ONLY | flags::DIRECTORY);
    check!(t, &name("O_DIRECTORY on a file is ENOTDIR"), r == Err(Errno::ENOTDIR), "{:?}", r);
    check!(t, &name("mkdir in a missing directory is ENOENT"), fs::mkdir(&path("nope/x")) == Err(Errno::ENOENT));
    check!(t, &name("removing a non-empty directory is ENOTEMPTY"), fs::unlink(&dir) == Err(Errno::ENOTEMPTY));
    check!(t, &name("removing a missing file is ENOENT"), fs::unlink(&path("nope")) == Err(Errno::ENOENT));
    check!(t, &name("readdir on a file is ENOTDIR"), fs::read_dir(&a).err() == Some(Errno::ENOTDIR));
    if let Ok(fd) = fs::open(&a, flags::READ_ONLY) {
        check!(t, &name("writing a read-only fd is EBADF"), ulib::write(fd, b"x") == Err(Errno::EBADF));
        let _ = fs::close(fd);
        let mut b = [0u8; 1];
        check!(t, &name("reading a closed fd is EBADF"), ulib::read(fd, &mut b) == Err(Errno::EBADF));
    }
    let mut fds = Vec::new();
    let full = loop {
        match fs::open(&a, flags::READ_ONLY) {
            Ok(fd) if fds.len() < 100 => fds.push(fd),
            other => break other,
        }
    };
    check!(t, &name("running out of fds is EMFILE"), full == Err(Errno::EMFILE), "{:?} after {}", full, fds.len());
    for fd in fds {
        let _ = fs::close(fd);
    }

    // Renaming.
    let b = path("b.txt");
    let _ = write_file(&b, b"old b");
    let r = fs::rename(&a, &b);
    let d = fs::read_file(&b);
    check!(t, &name("rename replaces the target"), r.is_ok() && d.as_deref() == Ok(&b"abc"[..]), "{:?} {:?}", r, d);
    check!(t, &name("the old name is gone"), fs::stat(&a) == Err(Errno::ENOENT));
    let r = fs::rename(&b, &path("d/moved.txt")).and_then(|_| fs::rename(&path("d/moved.txt"), &a));
    check!(t, &name("rename between directories"), r.is_ok() && fs::stat(&a).map(|s| s.size) == Ok(3), "{:?}", r);
    let _ = fs::mkdir(&path("d/inner"));
    check!(t, &name("rename a file over a directory is EISDIR"), fs::rename(&a, &path("d")) == Err(Errno::EISDIR));
    check!(t, &name("rename a directory over a file is ENOTDIR"), fs::rename(&path("d"), &a) == Err(Errno::ENOTDIR));
    let _ = fs::mkdir(&path("e"));
    check!(t, &name("rename over a non-empty directory is ENOTEMPTY"), fs::rename(&path("e"), &path("d")) == Err(Errno::ENOTEMPTY));
    check!(t, &name("moving a directory into itself is EINVAL"), fs::rename(&path("d"), &path("d/inner/x")) == Err(Errno::EINVAL));
    let r = fs::rename(&path("d/inner"), &path("e2")).and_then(|_| fs::rename(&path("e2"), &path("e")));
    check!(t, &name("rename a directory over an empty one"), r.is_ok() && fs::stat(&path("e")).is_ok(), "{:?}", r);
    let _ = fs::unlink(&path("e"));

    // Children inherit descriptors: that's how the shell redirects.
    let out = flags::WRITE_ONLY | flags::CREATE | flags::TRUNCATE;
    let r = run_redirected(ulib::STDOUT, &path("out"), out, &["say", "redirected"]);
    let d = fs::read_file(&path("out"));
    check!(t, &name("child output into a file"), r == exited(0) && d.as_deref() == Ok(&b"redirected\n"[..]), "{:?} {:?}", r, d);
    let r = run_redirected(ulib::STDIN, &a, flags::READ_ONLY, &["stdin-len"]);
    check!(t, &name("child input from a file"), r == exited(3), "{:?}", r);

    // An unlinked file lives on while open.
    let fd = fs::open(&a, flags::READ_ONLY);
    let gone = fs::unlink(&a);
    let d = fd.and_then(fs::read_to_end);
    check!(t, &name("unlinked file still readable while open"), gone.is_ok() && d.as_deref() == Ok(&b"abc"[..]), "{:?}", d);
    if let Ok(fd) = fd {
        let _ = fs::close(fd);
    }

    for p in [path("out"), path("d"), dir.clone()] {
        let _ = fs::unlink(&p);
    }
    check!(t, &name("cleanup"), fs::stat(&dir) == Err(Errno::ENOENT));
}

fn disk_free() -> (u64, u64) {
    fs::statfs("/disk").map_or((0, 0), |s| (s.free_blocks, s.free_inodes))
}

/// What only the disk does: persistence, big and sparse files, running out
/// of space, and giving every block back.
fn disk(t: &mut Suite) {
    // Counts the boots this disk has seen; tests/run.py boots twice on the
    // same image and checks the second boot sees the first.
    let boots = fs::read_file("/disk/boots")
        .ok()
        .and_then(|d| core::str::from_utf8(&d).ok()?.trim().parse::<u64>().ok())
        .unwrap_or(0);
    println!("# disk has seen {} earlier boot(s)", boots);
    let r = write_file("/disk/boots", alloc::format!("{}\n", boots + 1).as_bytes());
    check!(t, "/disk: boot counter updated", r.is_ok(), "{:?}", r);
    let before = disk_free();
    let stats = fs::statfs("/disk");
    check!(t, "/disk: statfs", stats.map_or(false, |s| s.block_size == 4096 && s.free_blocks > 0 && s.total_inodes > 0), "{:?}", stats);
    check!(t, "/disk: rename to another filesystem is EXDEV", fs::rename("/disk/boots", "/tmp/boots") == Err(Errno::EXDEV));
    let long = [b'n'; 249]; // the VFS allows 255, KFS stores 248
    let r = write_file(&alloc::format!("/disk/{}", core::str::from_utf8(&long).unwrap()), b"x");
    check!(t, "/disk: a 249-byte name is ENAMETOOLONG", r == Err(Errno::ENAMETOOLONG), "{:?}", r);

    // 5 MiB: past the direct and indirect blocks into the double-indirect ones.
    const BIG: usize = 5 << 20;
    let pattern = |i: usize| (i % 251) as u8 ^ (i / 4096) as u8;
    let data: Vec<u8> = (0..BIG).map(pattern).collect();
    let r = write_file("/disk/big", &data);
    drop(data);
    let back = fs::read_file("/disk/big");
    let same = back.as_ref().map_or(false, |d| d.len() == BIG && d.iter().enumerate().all(|(i, &b)| b == pattern(i)));
    check!(t, "/disk: 5 MiB file round-trips", r.is_ok() && same, "{:?}", r);
    drop(back);

    // A sparse file: the hole reads as zeros and takes no space.
    let free_before_sparse = disk_free().0;
    let sparse = fs::open("/disk/sparse", flags::READ_WRITE | flags::CREATE).and_then(|fd| {
        fs::seek(fd, 10 << 20, Whence::Start)?;
        ulib::write_all(fd, b"end")?;
        let mut head = [1u8; 4096];
        fs::seek(fd, 4096 * 100, Whence::Start)?;
        let n = ulib::read(fd, &mut head)?;
        let _ = fs::close(fd);
        Ok(n == 4096 && head.iter().all(|&b| b == 0))
    });
    let size = fs::stat("/disk/sparse").map(|s| s.size);
    let used = free_before_sparse - disk_free().0;
    check!(t, "/disk: sparse file", sparse == Ok(true) && size == Ok((10 << 20) + 3) && used <= 4, "{:?} {:?}, {} blocks", sparse, size, used);

    // Many entries: a directory bigger than one block.
    let _ = fs::mkdir("/disk/many");
    let mut ok = true;
    for i in 0..40 {
        ok &= write_file(&alloc::format!("/disk/many/f{:02}", i), b"").is_ok();
    }
    let listed: Vec<String> = fs::read_dir("/disk/many").map(|d| d.flatten().map(|e| String::from(e.name())).collect()).unwrap_or_default();
    let want: Vec<String> = (0..40).map(|i| alloc::format!("f{:02}", i)).collect();
    check!(t, "/disk: a directory of 40 entries", ok && listed == want, "{} listed", listed.len());
    for i in 0..40 {
        let _ = fs::unlink(&alloc::format!("/disk/many/f{:02}", i));
    }
    let r = fs::unlink("/disk/many");
    check!(t, "/disk: emptied directory can be removed", r.is_ok(), "{:?}", r);

    // Fill the disk: writes stop with ENOSPC, and everything comes back.
    let chunk = alloc::vec![0x5au8; 1 << 20];
    let fill = fs::open("/disk/fill", flags::WRITE_ONLY | flags::CREATE);
    let mut error = None;
    if let Ok(fd) = fill {
        for _ in 0..4096 {
            if let Err(e) = ulib::write_all(fd, &chunk) {
                error = Some(e);
                break;
            }
        }
        let _ = fs::close(fd);
    }
    check!(t, "/disk: a full disk is ENOSPC", error == Some(Errno::ENOSPC), "{:?}", error);
    check!(t, "/disk: statfs shows the disk full", disk_free().0 == 0, "{:?}", disk_free());

    // An unlinked file's blocks are freed only when it is closed.
    let held = fs::open("/disk/big", flags::READ_ONLY);
    for p in ["/disk/fill", "/disk/sparse", "/disk/big"] {
        let _ = fs::unlink(p);
    }
    let while_open = disk_free().0;
    if let Ok(fd) = held {
        let _ = fs::close(fd);
    }
    let after = disk_free();
    check!(t, "/disk: an open unlinked file keeps its blocks", while_open + 1280 <= after.0, "{} then {}", while_open, after.0);
    check!(t, "/disk: every block and inode is given back", after == before, "{:?} -> {:?}", before, after);
}

/// Spawn `selftest <args>` with its stdin/stdout pointed at the given fds.
fn spawn_with(stdin: Option<fs::Fd>, stdout: Option<fs::Fd>, args: &[&str]) -> Result<ulib::Pid, Errno> {
    let mut saved = Vec::new();
    for (target, fd) in [(ulib::STDIN, stdin), (ulib::STDOUT, stdout)] {
        if let Some(fd) = fd {
            saved.push((target, fs::dup(target, flags::CLOSE_ON_SPAWN)?));
            fs::dup2(fd, target)?;
        }
    }
    let pid = ulib::spawn(NAME, args);
    for (target, copy) in saved.into_iter().rev() {
        let _ = fs::dup2(copy, target);
        let _ = fs::close(copy);
    }
    pid
}

fn pipes(t: &mut Suite) {
    // In one process.
    match fs::pipe(0) {
        Ok((r, w)) => {
            let mut buf = [0u8; 16];
            let wrote = ulib::write(w, b"hello");
            let read = ulib::read(r, &mut buf);
            check!(t, "pipe passes data", wrote == Ok(5) && read == Ok(5) && &buf[..5] == b"hello", "{:?} {:?}", wrote, read);
            check!(t, "fstat of a pipe", fs::fstat(r).map(|s| s.kind) == Ok(Kind::Pipe));
            let _ = fs::close(w);
            check!(t, "closed write end reads as EOF", ulib::read(r, &mut buf) == Ok(0));
            let _ = fs::close(r);
        }
        Err(e) => check!(t, "pipe passes data", false, "{:?}", e),
    }
    if let Ok((r, w)) = fs::pipe(0) {
        let _ = fs::close(r);
        let res = ulib::write(w, b"x");
        check!(t, "writing with no reader is EPIPE", res == Err(Errno::EPIPE), "{:?}", res);
        let _ = fs::close(w);
    }
    check!(t, "pipe with bad flags is EINVAL", fs::pipe(1) == Err(Errno::EINVAL));

    // 1 MiB through a 64 KiB pipe: both sides block and wake many times.
    const BIG: usize = 1 << 20;
    if let Ok((r, w)) = fs::pipe(flags::CLOSE_ON_SPAWN) {
        let child = spawn_with(None, Some(w), &["write-bytes", "1048576"]);
        let _ = fs::close(w);
        let got = fs::read_to_end(r).map(|d| (d.len(), d.iter().all(|&b| b == b'x')));
        let _ = fs::close(r);
        let status = child.and_then(ulib::wait);
        check!(t, "1 MiB from a child through a pipe", got == Ok((BIG, true)) && status == exited(0), "{:?} {:?}", got, status);
    }
    if let Ok((r, w)) = fs::pipe(flags::CLOSE_ON_SPAWN) {
        let child = spawn_with(Some(r), None, &["stdin-len"]);
        let _ = fs::close(r);
        let chunk = [b'y'; 4096];
        let mut ok = true;
        for _ in 0..(BIG / chunk.len()) {
            ok &= ulib::write_all(w, &chunk).is_ok();
        }
        let _ = fs::close(w);
        let status = child.and_then(ulib::wait);
        check!(t, "1 MiB to a child through a pipe", ok && status == exited(BIG as i32), "{:?}", status);
    }

    // Close-on-spawn descriptors aren't inherited, whether from open or dup.
    if let Ok(fd) = fs::open("/bin/selftest", flags::READ_ONLY) {
        let copy = fs::dup(fd, flags::CLOSE_ON_SPAWN);
        let text = copy.map(|c| alloc::format!("{}", c)).unwrap_or_default();
        let r = run_child(&["fd-open", &text]);
        check!(t, "close-on-spawn dup is not inherited", copy.is_ok() && r == exited(0), "{:?} {:?}", copy, r);
        check!(t, "dup with bad flags is EINVAL", fs::dup(fd, 1) == Err(Errno::EINVAL));
        for c in [Ok(fd), copy].into_iter().flatten() {
            let _ = fs::close(c);
        }
    }
    for (cloexec, expect) in [(flags::CLOSE_ON_SPAWN, 0), (0, 1)] {
        let fd = fs::open("/bin/selftest", flags::READ_ONLY | cloexec);
        let text = fd.map(|fd| alloc::format!("{}", fd)).unwrap_or_default();
        let r = run_child(&["fd-open", &text]);
        let name = if cloexec != 0 { "close-on-spawn fd is not inherited" } else { "plain fd is inherited" };
        check!(t, name, r == exited(expect), "{:?}", r);
        if let Ok(fd) = fd {
            let _ = fs::close(fd);
        }
    }
}

fn signals(t: &mut Suite) {
    use ulib::signal::{SIGINT, SIGKILL, SIGTERM};
    let killed = |sig| Ok(ExitStatus::Killed(sig));

    // A process that never makes a system call still dies (at a timer tick).
    if let Ok(pid) = ulib::spawn(NAME, &["spin-forever"]) {
        ulib::sleep_ms(20);
        let start = ulib::uptime_ms();
        let sent = ulib::kill(pid as i64, SIGKILL);
        let r = ulib::wait(pid);
        let took = ulib::uptime_ms() - start;
        check!(t, "SIGKILL stops a spinning process", sent.is_ok() && r == killed(SIGKILL) && took < 200, "{:?} {:?} {} ms", sent, r, took);
    }
    // A sleeping one is woken to die.
    if let Ok(pid) = ulib::spawn(NAME, &["sleep-forever"]) {
        ulib::sleep_ms(20);
        let start = ulib::uptime_ms();
        let _ = ulib::kill(pid as i64, SIGTERM);
        let r = ulib::wait(pid);
        let took = ulib::uptime_ms() - start;
        check!(t, "SIGTERM interrupts sleep", r == killed(SIGTERM) && took < 200, "{:?} {} ms", r, took);
    }
    // So is one blocked reading a pipe nobody writes to.
    if let Ok((r_end, w_end)) = fs::pipe(flags::CLOSE_ON_SPAWN) {
        let child = spawn_with(Some(r_end), None, &["read-forever"]);
        ulib::sleep_ms(20);
        if let Ok(pid) = child {
            let _ = ulib::kill(pid as i64, SIGINT);
        }
        let r = child.and_then(ulib::wait);
        check!(t, "SIGINT interrupts a blocking read", r == killed(SIGINT), "{:?}", r);
        let _ = fs::close(r_end);
        let _ = fs::close(w_end);
    }
    // A whole process group at once.
    let mut pids = Vec::new();
    for _ in 0..3 {
        if let Ok(pid) = ulib::spawn(NAME, &["sleep-forever"]) {
            let group = pids.first().copied().unwrap_or(pid);
            let _ = ulib::setpgid(pid, group);
            pids.push(pid);
        }
    }
    let sent = ulib::kill(-(pids[0] as i64), SIGTERM);
    let all = pids.iter().all(|&pid| ulib::wait(pid) == killed(SIGTERM));
    check!(t, "kill a process group", sent.is_ok() && pids.len() == 3 && all, "{:?}", sent);

    // CPU faults turn into the usual signals.
    let r = run_child(&["illegal"]);
    check!(t, "invalid opcode is SIGILL", r == killed(ulib::signal::SIGILL), "{:?}", r);
    let r = run_child(&["divide"]);
    check!(t, "divide by zero is SIGFPE", r == killed(ulib::signal::SIGFPE), "{:?}", r);
    let r = run_child(&["set-ac", "100"]);
    check!(t, "RFLAGS.AC from user mode doesn't reach interrupt handlers", r == exited(0), "{:?}", r);

    // Errors.
    check!(t, "killing init is EPERM", ulib::kill(1, SIGTERM) == Err(Errno::EPERM));
    check!(t, "killing a missing process is ESRCH", ulib::kill(999_999, SIGTERM) == Err(Errno::ESRCH));
    check!(t, "signal 0 is EINVAL", ulib::kill(ulib::getpid() as i64, 0) == Err(Errno::EINVAL));
    check!(t, "signal 64 is EINVAL", ulib::kill(ulib::getpid() as i64, 64) == Err(Errno::EINVAL));
    check!(t, "setpgid of a missing process is ESRCH", ulib::setpgid(999_999, 1) == Err(Errno::ESRCH));

    // The clock: after 2020, and moving.
    let t1 = ulib::time_ms();
    ulib::sleep_ms(100);
    let t2 = ulib::time_ms();
    check!(t, "time_ms is plausible", t1 > 1_577_836_800_000, "{}", t1);
    check!(t, "time_ms advances", (90..300).contains(&(t2 - t1)), "{} ms", t2 - t1);
}

fn leaks(t: &mut Suite) {
    // Children we already collected may still await reaping; let that happen.
    ulib::sleep_ms(50);
    let before = ulib::sysinfo().tasks;
    let r = run_child(&["orphan"]);
    ulib::sleep_ms(50); // let the kernel reap
    let after = ulib::sysinfo().tasks;
    check!(t, "orphaned child leaves no task entry", r == exited(0) && after == before, "tasks {} -> {}", before, after);

    // A first round lets the kernel heap grow to its working size; after the
    // second, every page must be back.
    let round = || {
        for _ in 0..8 {
            let _ = run_child(&["exit", "0"]);
        }
        let _ = run_child(&["hog", "32"]);
        ulib::sleep_ms(20);
    };
    let round = || {
        round();
        let _ = run_child(&["hog-then-exec", "16"]);
        let _ = run_child(&["catch-many"]);
    };
    round();
    let before = ulib::sysinfo().free_memory;
    round();
    let after = ulib::sysinfo().free_memory;
    check!(t, "processes return all their memory", after == before, "free {} -> {} bytes", before, after);
}

// ------------------------------------------------------ exec and handlers

/// Handler calls so far, and the last signal one got.
static COUNT: AtomicU32 = AtomicU32::new(0);
static LAST: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);

extern "C" fn count(sig: i32) {
    COUNT.fetch_add(1, Ordering::Relaxed);
    LAST.store(sig as u32, Ordering::Relaxed);
}

extern "C" fn finish(_: i32) {
    DONE.store(true, Ordering::Relaxed);
}

extern "C" fn exit_42(_: i32) {
    ulib::exit(42);
}

/// Counts, and wrecks registers the interrupted code is using.
extern "C" fn count_and_clobber(sig: i32) {
    count(sig);
    unsafe {
        core::arch::asm!("xor ecx, ecx", "xor r11d, r11d", "pxor xmm0, xmm0", out("rcx") _, out("r11") _, out("xmm0") _);
    }
}

/// Steps over the 2-byte `ud2` that raised the signal.
extern "C" fn skip_ud2(sig: i32, _: usize, regs: *mut Regs) {
    count(sig);
    unsafe { (*regs).rip += 2 };
}

/// Where `bad-mxcsr` resumes: exit 0 if MXCSR's reserved bits are clear.
extern "C" fn exit_mxcsr_ok() -> ! {
    let mut mxcsr: u32 = 0;
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut mxcsr) };
    ulib::exit((mxcsr >> 16 != 0) as i32)
}

fn handle(sig: u8, f: extern "C" fn(i32)) {
    ulib::signal::set_handler(sig, Handler::Call(f)).expect("set_handler");
}

fn raise(sig: u8) {
    let _ = ulib::kill(ulib::getpid() as i64, sig);
}

fn counted() -> u32 {
    COUNT.load(Ordering::Relaxed)
}

/// `sigreturn` with a hand-made frame, as a broken or hostile program might.
fn fake_sigreturn(frame: &ulib::signal::Frame) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rsp, {sp}",
            "mov eax, {n}",
            "syscall",
            "ud2",
            sp = in(reg) frame as *const _ as u64 + 8,
            n = const sys::SIGRETURN,
            options(noreturn),
        )
    }
}

fn blank_frame() -> Box<ulib::signal::Frame> {
    Box::new(ulib::signal::Frame { restorer: 0, sig: 0, regs: Regs::default(), mask: 0, _pad: [0; 2], fpu: [0; 512] })
}

/// Child modes for the exec and handler checks: Some(exit code).
fn signal_mode(args: &[&str]) -> Option<i32> {
    let code = match args {
        ["exec", prog, rest @ ..] => 100 + ulib::exec(prog, rest).0 as i32,
        ["exec-keeps-pid"] => 100 + ulib::exec(NAME, &["pid-is", &format!("{}", ulib::getpid())]).0 as i32,
        ["pid-is", pid] => (format!("{}", ulib::getpid()) != *pid) as i32,
        ["exec-fd", cloexec] => {
            let how = if *cloexec == "1" { flags::CLOSE_ON_SPAWN } else { 0 };
            let Ok(fd) = fs::open("/dev/null", how) else { return Some(99) };
            100 + ulib::exec(NAME, &["fd-open", &format!("{}", fd)]).0 as i32
        }
        ["hog-then-exec", mib] => {
            let v = vec![1u8; mib.parse::<usize>().unwrap_or(0) << 20];
            black_box(&v);
            100 + ulib::exec(NAME, &["exit", "0"]).0 as i32
        }
        ["exec-handlers"] => {
            handle(SIGUSR1, count);
            let _ = ulib::signal::set_handler(SIGUSR2, Handler::Ignore);
            100 + ulib::exec(NAME, &["sleep-forever"]).0 as i32
        }
        ["catch-self"] => {
            handle(SIGUSR1, count);
            raise(SIGUSR1); // runs before kill returns
            (counted() != 1 || LAST.load(Ordering::Relaxed) != SIGUSR1 as u32) as i32
        }
        ["catch-many"] => {
            handle(SIGUSR1, count);
            for _ in 0..100 {
                raise(SIGUSR1);
            }
            (counted() != 100) as i32
        }
        ["mask"] => {
            handle(SIGUSR1, count);
            ulib::signal::block(ulib::signal::bit(SIGUSR1));
            raise(SIGUSR1);
            let while_blocked = counted();
            let old = ulib::signal::unblock(ulib::signal::bit(SIGUSR1));
            let ok = while_blocked == 0 && counted() == 1 && old & ulib::signal::bit(SIGUSR1) != 0;
            !ok as i32
        }
        ["ignore"] => {
            let _ = ulib::signal::set_handler(SIGUSR1, Handler::Ignore);
            raise(SIGUSR1);
            raise(SIGCHLD); // ignored by default
            0
        }
        // Signals arriving at any instruction (from the timer interrupt)
        // must leave every register as it was, rcx and r11 included.
        ["registers"] => {
            handle(SIGUSR1, count_and_clobber);
            handle(SIGUSR2, finish);
            let (a, b) = (0x1111_2222_3333_4444u64, 0x5555_6666_7777_8888u64);
            while !DONE.load(Ordering::Relaxed) {
                let (rcx, r11, xmm): (u64, u64, u64);
                unsafe {
                    core::arch::asm!(
                        "mov rcx, {a}", "mov r11, {b}", "movq xmm0, {b}",
                        "2:", "dec {n}", "jnz 2b",
                        "movq {x}, xmm0",
                        a = in(reg) a, b = in(reg) b, n = inout(reg) 200_000u64 => _, x = out(reg) xmm,
                        out("rcx") rcx, out("r11") r11, out("xmm0") _,
                    );
                }
                if (rcx, r11, xmm) != (a, b, b) {
                    return Some(1);
                }
            }
            if counted() == 0 { 2 } else { 0 }
        }
        ["eintr"] => {
            handle(SIGUSR1, count);
            let mut buf = [0u8; 8];
            let r = ulib::read(ulib::STDIN, &mut buf);
            (r != Err(Errno::EINTR) || counted() != 1) as i32
        }
        ["suspend"] => {
            handle(SIGUSR1, count);
            ulib::signal::block(ulib::signal::bit(SIGUSR1));
            ulib::signal::suspend(0);
            let ok = counted() == 1 && ulib::signal::blocked() & ulib::signal::bit(SIGUSR1) != 0;
            !ok as i32
        }
        ["segv-handler"] => {
            handle(signal::SIGSEGV, exit_42);
            unsafe { core::ptr::write_volatile(0xffff_ffff_8000_0000 as *mut u64, 0) };
            0
        }
        ["skip-ud2"] => {
            let _ = ulib::signal::set_handler(SIGILL, Handler::WithRegs(skip_ud2));
            unsafe { core::arch::asm!("ud2") };
            (counted() != 1) as i32
        }
        ["chld"] => {
            handle(SIGCHLD, count);
            let r = run_child(&["exit", "0"]);
            (r != exited(0) || counted() != 1 || LAST.load(Ordering::Relaxed) != SIGCHLD as u32) as i32
        }
        ["bad-sigreturn"] => {
            let mut frame = blank_frame();
            frame.regs.rip = 0xffff_8000_0000_0000; // kernel space
            frame.regs.rsp = 0x1000;
            fake_sigreturn(&frame)
        }
        ["bad-mxcsr"] => {
            // Every MXCSR bit set, reserved ones included: the kernel must
            // drop those (a real CPU's fxrstor faults on them; QEMU's
            // doesn't, hence the check on the value that arrives).
            #[repr(C, align(16))]
            struct Fx([u8; 512]);
            let mut fx = Fx([0; 512]);
            unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) fx.0.as_mut_ptr()) };
            fx.0[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
            let stack = Box::leak(vec![0u8; 16384].into_boxed_slice());
            let mut frame = blank_frame();
            frame.fpu = fx.0;
            frame.regs.rip = exit_mxcsr_ok as *const () as u64;
            frame.regs.rsp = ((stack.as_ptr() as u64 + 16384) & !0xf) - 8;
            fake_sigreturn(&frame)
        }
        _ => return None,
    };
    Some(code)
}

fn exec(t: &mut Suite) {
    let r = run_child(&["exec", NAME, "exit", "7"]);
    check!(t, "exec runs the new program", r == exited(7), "{:?}", r);
    let r = run_child(&["exec", NAME, "argcheck", "one", "two words", ""]);
    check!(t, "exec passes arguments", r == exited(0), "{:?}", r);
    let r = run_child(&["exec-keeps-pid"]);
    check!(t, "exec keeps the pid", r == exited(0), "{:?}", r);
    let r = run_child(&["exec", "no-such-program"]);
    check!(t, "exec of a missing program fails and returns", r == exited(100 + Errno::ENOENT.0 as i32), "{:?}", r);
    let r = run_child(&["exec-fd", "0"]);
    check!(t, "exec keeps descriptors", r == exited(1), "{:?}", r);
    let r = run_child(&["exec-fd", "1"]);
    check!(t, "exec closes CLOSE_ON_SPAWN descriptors", r == exited(0), "{:?}", r);

    // In this process: failures must leave it running as it was.
    let r = ulib::exec("no-such-program", &[]);
    check!(t, "failed exec returns ENOENT", r == Errno::ENOENT, "{:?}", r);
    let not_elf = "/tmp/not-elf";
    let r = write_file(not_elf, b"#!/bin/sh\necho hi\n").map(|_| ulib::exec(not_elf, &[]));
    check!(t, "exec of a non-ELF file is ENOEXEC", r == Ok(Errno::ENOEXEC), "{:?}", r);
    let _ = fs::unlink(not_elf);
    let pairs = [[0u64; 2]; ulib::MAX_ARGS + 1];
    let r = unsafe {
        let name = NAME.as_ptr() as u64;
        syscall6(sys::EXEC, name, NAME.len() as u64, pairs.as_ptr() as u64, pairs.len() as u64, 0, 0)
    };
    check!(t, "exec with too many arguments is E2BIG", r == -Errno::E2BIG.0, "got {}", r);

    // Handlers don't survive exec; ignored signals stay ignored.
    if let Ok(pid) = ulib::spawn(NAME, &["exec-handlers"]) {
        ulib::sleep_ms(50);
        let _ = ulib::kill(pid as i64, SIGUSR2);
        ulib::sleep_ms(20);
        let _ = ulib::kill(pid as i64, SIGUSR1);
        let r = ulib::wait(pid);
        check!(t, "exec resets handlers, keeps ignored signals", r == Ok(ExitStatus::Killed(SIGUSR1)), "{:?}", r);
    }
}

fn handlers(t: &mut Suite) {
    use ulib::signal::{bit, SIGKILL, SIGSTOP};
    let r = run_child(&["catch-self"]);
    check!(t, "a handler catches a signal", r == exited(0), "{:?}", r);
    let r = run_child(&["catch-many"]);
    check!(t, "100 signals, 100 handler calls", r == exited(0), "{:?}", r);
    let r = run_child(&["mask"]);
    check!(t, "a blocked signal waits until unblocked", r == exited(0), "{:?}", r);
    let r = run_child(&["ignore"]);
    check!(t, "ignored signals, and SIGCHLD by default, don't kill", r == exited(0), "{:?}", r);
    let r = run_child(&["segv-handler"]);
    check!(t, "a SIGSEGV handler runs on a fault", r == exited(42), "{:?}", r);
    let r = run_child(&["skip-ud2"]);
    check!(t, "a handler can change the registers it returns to", r == exited(0), "{:?}", r);
    let r = run_child(&["chld"]);
    check!(t, "SIGCHLD when a child exits", r == exited(0), "{:?}", r);
    let r = run_child(&["bad-sigreturn"]);
    check!(t, "sigreturn into the kernel is SIGSEGV", r == Ok(KILLED_SEGV), "{:?}", r);
    let r = run_child(&["bad-mxcsr"]);
    check!(t, "sigreturn clears reserved MXCSR bits", r == exited(0), "{:?}", r);

    // Signals from another process, arriving at any instruction.
    if let Ok(pid) = ulib::spawn(NAME, &["registers"]) {
        for _ in 0..40 {
            ulib::sleep_ms(3);
            let _ = ulib::kill(pid as i64, SIGUSR1);
        }
        let _ = ulib::kill(pid as i64, SIGUSR2);
        let r = ulib::wait(pid);
        check!(t, "handlers preserve every register (rcx, r11, xmm0)", r == exited(0), "{:?}", r);
    }
    if let Ok((r_end, w_end)) = fs::pipe(flags::CLOSE_ON_SPAWN) {
        let child = spawn_with(Some(r_end), None, &["eintr"]);
        ulib::sleep_ms(30);
        if let Ok(pid) = child {
            let _ = ulib::kill(pid as i64, SIGUSR1);
        }
        let r = child.and_then(ulib::wait);
        check!(t, "a handled signal makes a blocking read EINTR", r == exited(0), "{:?}", r);
        let _ = fs::close(r_end);
        let _ = fs::close(w_end);
    }
    if let Ok(pid) = ulib::spawn(NAME, &["suspend"]) {
        ulib::sleep_ms(30);
        let _ = ulib::kill(pid as i64, SIGUSR1);
        let r = ulib::wait(pid);
        check!(t, "sigsuspend waits for a handler, then restores the mask", r == exited(0), "{:?}", r);
    }

    // Errors, in this process.
    let r = ulib::signal::set_handler(SIGKILL, Handler::Call(count));
    check!(t, "SIGKILL can't be caught", matches!(r, Err(Errno::EINVAL)), "{:?}", r);
    let r = ulib::signal::set_handler(SIGSTOP, Handler::Ignore);
    check!(t, "SIGSTOP can't be ignored", matches!(r, Err(Errno::EINVAL)), "{:?}", r);
    let r = ulib::signal::set_handler(32, Handler::Default);
    check!(t, "sigaction of signal 32 is EINVAL", matches!(r, Err(Errno::EINVAL)), "{:?}", r);
    let action = [count as *const () as u64, 0, 0, 0]; // no SA_RESTORER
    let r = raw(sys::SIGACTION, SIGUSR1 as u64, action.as_ptr() as u64, 0);
    check!(t, "a handler without a restorer is EINVAL", r == -Errno::EINVAL.0, "got {}", r);
    let r = raw(sys::SIGACTION, SIGUSR1 as u64, BAD_PTR, 0);
    check!(t, "sigaction from a bad pointer is EFAULT", r == -Errno::EFAULT.0, "got {}", r);
    let set: u64 = 0;
    let r = raw(sys::SIGPROCMASK, 7, &set as *const u64 as u64, 0);
    check!(t, "sigprocmask with a bad operation is EINVAL", r == -Errno::EINVAL.0, "got {}", r);
    let old = ulib::signal::set_blocked(u64::MAX);
    let all = ulib::signal::blocked();
    ulib::signal::set_blocked(old);
    check!(t, "SIGKILL and SIGSTOP can't be blocked", all & (bit(SIGKILL) | bit(SIGSTOP)) == 0 && all & bit(SIGUSR1) != 0, "{:#x}", all);
}
