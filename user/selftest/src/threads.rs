//! Threads: several flows of control sharing one process.

use crate::{exited, run_child, Suite, BAD_PTR, KILLED_SEGV, NAME};
use alloc::{format, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ulib::signal::{self, Handler, SIGUSR1};
use ulib::sync::Mutex;
use ulib::{prot, thread, ExitStatus};

/// Child modes: Some(exit code).
pub fn mode(args: &[&str]) -> Option<i32> {
    let code = match args {
        // `exit` from a thread ends the whole process.
        ["thread-exits-process"] => {
            let _t = thread::spawn(|| ulib::exit(7));
            ulib::sleep_ms(10_000);
            1
        }
        // The first thread ends; the process lives on in the other, and
        // ends with the last thread's exit code.
        ["first-thread-ends"] => {
            let _t = thread::spawn(|| {
                ulib::sleep_ms(50);
                thread::exit_thread(5)
            });
            thread::exit_thread(1)
        }
        ["thread-faults"] => {
            let _t = thread::spawn(|| unsafe { core::ptr::write_volatile(BAD_PTR as *mut u64, 1) });
            ulib::sleep_ms(10_000);
            1
        }
        ["threads-sleep", n] => {
            for _ in 0..n.parse().unwrap_or(0) {
                let _ = thread::spawn(|| ulib::sleep_ms(3_600_000));
            }
            ulib::sleep_ms(3_600_000);
            1
        }
        ["thread-gets-signal"] => thread_gets_signal(),
        // exec from the first thread, or from another one, while more
        // threads run: the new program starts alone.
        ["exec-with-threads", from] => {
            for _ in 0..3 {
                let _ = thread::spawn(|| loop {
                    ulib::sleep_ms(1)
                });
            }
            if *from == "main" {
                return Some(100 + ulib::exec(NAME, &["exit", "3"]).0 as i32);
            }
            let _ = thread::spawn(|| ulib::exit(100 + ulib::exec(NAME, &["exit", "3"]).0 as i32));
            ulib::sleep_ms(10_000);
            1
        }
        ["unmap-under-reader"] => unmap_under_reader(),
        ["thread-churn"] => {
            let mut sum = 0;
            for i in 0..20u64 {
                let handles: Vec<_> = (0..3).filter_map(|j| thread::spawn(move || i * 3 + j).ok()).collect();
                sum += handles.into_iter().map(|h| h.join()).sum::<u64>();
            }
            (sum != (0..60).sum::<u64>()) as i32
        }
        _ => return None,
    };
    Some(code)
}

static HANDLED_BY: AtomicU64 = AtomicU64::new(0);
static READY: AtomicBool = AtomicBool::new(false);

extern "C" fn note_thread(_: i32) {
    HANDLED_BY.store(thread::id(), Ordering::Relaxed);
}

/// A signal sent to the process goes to a thread that doesn't block it.
fn thread_gets_signal() -> i32 {
    let _ = signal::set_handler(SIGUSR1, Handler::Call(note_thread));
    signal::block(signal::bit(SIGUSR1)); // new threads start with this mask
    let Ok(t) = thread::spawn(|| {
        signal::unblock(signal::bit(SIGUSR1));
        READY.store(true, Ordering::Release);
        let end = ulib::uptime_ms() + 2000;
        while HANDLED_BY.load(Ordering::Relaxed) == 0 && ulib::uptime_ms() < end {
            ulib::sleep_ms(1);
        }
        thread::id()
    }) else {
        return 2;
    };
    while !READY.load(Ordering::Acquire) {
        ulib::yield_now();
    }
    let _ = ulib::kill(ulib::getpid() as i64, SIGUSR1);
    let tid = t.join();
    (HANDLED_BY.load(Ordering::Relaxed) != tid) as i32
}

static READS: AtomicU64 = AtomicU64::new(0);

/// One thread reads a page in a loop (no system calls, so nothing else
/// flushes its TLB) while another unmaps it: the reader must fault.
fn unmap_under_reader() -> i32 {
    let Ok(page) = ulib::mmap(4096, prot::READ | prot::WRITE) else { return 2 };
    let at = page as u64;
    let _reader = thread::spawn(move || loop {
        unsafe { core::ptr::read_volatile(at as *const u64) };
        READS.fetch_add(1, Ordering::Relaxed);
    });
    let end = ulib::uptime_ms() + 1000;
    while READS.load(Ordering::Relaxed) < 1000 && ulib::uptime_ms() < end {
        core::hint::spin_loop();
    }
    unsafe { ulib::munmap(page, 4096).unwrap() };
    // A reader that still sees the page keeps counting.
    ulib::sleep_ms(300);
    0
}

pub fn suite(t: &mut Suite) {
    let pid = ulib::getpid();
    let r = thread::spawn(move || (thread::id(), ulib::getpid())).map(|h| (h.tid(), h.join()));
    check!(t, "a thread runs and returns its result",
           matches!(r, Ok((tid, (id, p))) if tid == id && tid != pid && p == pid), "{:?} (pid {})", r, pid);

    // Shared memory: an atomic, and a counter behind a Mutex.
    static HITS: AtomicU64 = AtomicU64::new(0);
    static COUNTER: Mutex<u64> = Mutex::new(0);
    HITS.store(0, Ordering::Relaxed);
    let workers: Vec<_> = (0..4)
        .filter_map(|_| {
            thread::spawn(|| {
                for _ in 0..20_000 {
                    HITS.fetch_add(1, Ordering::Relaxed);
                    *COUNTER.lock() += 1;
                }
            })
            .ok()
        })
        .collect();
    let n = workers.len();
    workers.into_iter().for_each(|w| w.join());
    let (hits, count) = (HITS.load(Ordering::Relaxed), *COUNTER.lock());
    check!(t, "threads share memory; Mutex excludes", n == 4 && hits == 80_000 && count == 80_000,
           "{} threads, {} atomic, {} locked", n, hits, count);

    // The heap is shared and locked.
    let workers: Vec<_> = (0..4u64)
        .filter_map(|i| {
            thread::spawn(move || {
                let mut sum = 0;
                for j in 0..500 {
                    let v: Vec<u64> = (0..(j % 64)).map(|k| k * i).collect();
                    sum += v.iter().sum::<u64>();
                    let s = format!("{}-{}", i, j);
                    sum += s.len() as u64;
                }
                sum
            })
            .ok()
        })
        .collect();
    let sums: Vec<u64> = workers.into_iter().map(|w| w.join()).collect();
    let expect = |i: u64| (0..500u64).map(|j| (0..(j % 64)).map(|k| k * i).sum::<u64>() + format!("{}-{}", i, j).len() as u64).sum::<u64>();
    check!(t, "threads allocate from one heap at once", sums.len() == 4 && sums.iter().enumerate().all(|(i, &s)| s == expect(i as u64)),
           "{:?}", sums);

    fs_base(t);
    parallel(t);

    let r = run_child(&["thread-churn"]);
    check!(t, "60 threads started and joined", r == exited(0), "{:?}", r);
    let r = run_child(&["thread-exits-process"]);
    check!(t, "exit in any thread ends the process", r == exited(7), "{:?}", r);
    let r = run_child(&["first-thread-ends"]);
    check!(t, "the process ends with its last thread", r == exited(5), "{:?}", r);
    let r = run_child(&["thread-faults"]);
    check!(t, "a fault in a thread kills the process", r == Ok(KILLED_SEGV), "{:?}", r);
    let r = run_child(&["thread-gets-signal"]);
    check!(t, "a signal goes to a thread that doesn't block it", r == exited(0), "{:?}", r);
    let r = run_child(&["exec-with-threads", "main"]);
    check!(t, "exec ends the other threads", r == exited(3), "{:?}", r);
    let r = run_child(&["exec-with-threads", "other"]);
    check!(t, "exec from a thread other than the first", r == exited(3), "{:?}", r);

    let before = ulib::sysinfo();
    let r = run_child(&["unmap-under-reader"]);
    let shootdowns = ulib::sysinfo().tlb_user_shootdowns - before.tlb_user_shootdowns;
    check!(t, "munmap reaches a thread running on another CPU", r == Ok(KILLED_SEGV), "{:?}", r);
    if before.cpus > 1 {
        check!(t, "it took a user TLB shootdown", shootdowns > 0, "{} shootdowns", shootdowns);
    }

    // Killing a process ends all its threads, and frees them.
    ulib::sleep_ms(50);
    let tasks = ulib::sysinfo().tasks;
    if let Ok(child) = ulib::spawn(NAME, &["threads-sleep", "5"]) {
        ulib::sleep_ms(50);
        let during = ulib::sysinfo().tasks;
        let _ = ulib::kill(child as i64, signal::SIGTERM);
        let r = ulib::wait(child);
        ulib::sleep_ms(50);
        let after = ulib::sysinfo().tasks;
        check!(t, "a signal ends every thread of a process", r == Ok(ExitStatus::Killed(signal::SIGTERM)) && during == tasks + 6 && after == tasks,
               "{:?}, tasks {} -> {} -> {}", r, tasks, during, after);
    }
}

/// FS base (thread-local storage) is per thread and survives switches.
fn fs_base(t: &mut Suite) {
    static MINE: [u64; 2] = [0x1111, 0];
    static THEIRS: [u64; 2] = [0x2222, 0];
    let check_own = |block: &'static [u64; 2]| -> bool {
        if thread::set_fs_base(block.as_ptr() as u64).is_err() {
            return false;
        }
        (0..200).all(|_| {
            ulib::yield_now();
            unsafe { thread::read_fs(0) == block[0] }
        })
    };
    let other = thread::spawn(move || check_own(&THEIRS));
    let mine = check_own(&MINE);
    let theirs = other.map(|h| h.join());
    let _ = thread::set_fs_base(0);
    check!(t, "each thread has its own FS base", mine && theirs == Ok(true), "here {}, there {:?}", mine, theirs);
}

/// Busy threads run on several CPUs at once.
fn parallel(t: &mut Suite) {
    let cpus = ulib::sysinfo().cpus;
    if cpus < 2 {
        return;
    }
    let n = cpus.min(4);
    let start = ulib::uptime_ms();
    let workers: Vec<_> = (0..n)
        .filter_map(|_| {
            thread::spawn(|| {
                let end = ulib::uptime_ms() + 300;
                while ulib::uptime_ms() < end {
                    core::hint::spin_loop();
                }
            })
            .ok()
        })
        .collect();
    workers.into_iter().for_each(|w| w.join());
    let took = ulib::uptime_ms() - start;
    let limit = 150 * (n + 1);
    check!(t, "busy threads run in parallel", took < limit, "{} x 300 ms took {} ms (limit {})", n, took, limit);
}
