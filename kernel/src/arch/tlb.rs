//! TLB shootdown: making other CPUs forget a mapping that changed.
//!
//! Each CPU caches page translations in its TLB. Kernel mappings are global
//! (they survive CR3 switches), so after the kernel unmaps one, any CPU that
//! used it may still translate through the old entry. Before the frame
//! behind it can be reused, every CPU must flush it: the one making the
//! change does so itself, the others get an IPI and acknowledge.
//!
//! User mappings only need flushing where their page tables are loaded: the
//! CPUs running a thread of that process. Each CPU records the page tables it
//! has loaded (`set_active`, before every CR3 switch), and `shootdown_user`
//! interrupts only the CPUs whose entry matches. A CPU that loads those page
//! tables later can't hold stale entries, since loading CR3 flushes
//! non-global ones.
//!
//! One shootdown runs at a time (`BUSY`). The initiator waits for the
//! acknowledgements with interrupts off, so a second initiator spinning for
//! `BUSY`, also with interrupts off, couldn't take the first one's IPI:
//! instead it handles the pending request itself while it spins. For the
//! same reason the caller must not hold a `SpinLock`: another CPU may be
//! spinning on it with interrupts off, and would never acknowledge.

use super::interrupts::VECTOR_TLB_SHOOTDOWN;
use super::{apic, cpu, percpu, smp};
use crate::mm::PAGE_SIZE;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static BUSY: AtomicBool = AtomicBool::new(false);
/// The current request: its number and range. A CPU has done request `n`
/// once its `ACKED` entry is `n`.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static START: AtomicU64 = AtomicU64::new(0);
static END: AtomicU64 = AtomicU64::new(0);
static ACKED: [AtomicU64; percpu::MAX_CPUS] = [const { AtomicU64::new(0) }; percpu::MAX_CPUS];
/// The page tables (PML4 address) each CPU has loaded.
static ACTIVE: [AtomicU64; percpu::MAX_CPUS] = [const { AtomicU64::new(0) }; percpu::MAX_CPUS];
static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);
static USER_SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);
/// Flushes done for another CPU's request: one per other CPU per kernel
/// shootdown, and one per CPU interrupted for a user one.
static REMOTE_FLUSHES: AtomicU64 = AtomicU64::new(0);

/// Past this many pages, flushing everything is cheaper than page by page.
const FULL_FLUSH_PAGES: u64 = 64;
const CR4_PGE: u64 = 1 << 7;

fn flush_local(start: u64, end: u64) {
    if (end - start) / PAGE_SIZE > FULL_FLUSH_PAGES {
        // Reloading CR3 keeps global entries; toggling PGE drops them too.
        let cr4 = cpu::read_cr4();
        unsafe {
            if cr4 & CR4_PGE != 0 {
                cpu::write_cr4(cr4 & !CR4_PGE);
                cpu::write_cr4(cr4);
            } else {
                cpu::write_cr3(cpu::read_cr3());
            }
        }
    } else {
        let mut page = start & !(PAGE_SIZE - 1);
        while page < end {
            unsafe { cpu::invlpg(page) };
            page += PAGE_SIZE;
        }
    }
}

/// Do the current request on this CPU, if it hasn't yet. Interrupts off.
fn catch_up() {
    let me = percpu::index();
    // The range is written before the generation, and can't change again
    // until every CPU (this one included) has acknowledged it.
    let generation = GENERATION.load(Ordering::Acquire);
    if ACKED[me].load(Ordering::Relaxed) != generation {
        flush_local(START.load(Ordering::Relaxed), END.load(Ordering::Relaxed));
        REMOTE_FLUSHES.fetch_add(1, Ordering::Relaxed);
        ACKED[me].store(generation, Ordering::Release);
    }
}

/// The shootdown IPI.
pub fn handle_ipi() {
    catch_up();
}

/// Load page tables `pml4` on this CPU. Interrupts off.
pub fn switch_to(pml4: u64) {
    // Published before the switch: a `shootdown_user` that misses it changed
    // the page tables before this CPU loads them (the fence orders the two).
    ACTIVE[percpu::index()].store(pml4, Ordering::SeqCst);
    unsafe { cpu::write_cr3(pml4) };
}

/// Flush kernel mappings in [start, end) from every CPU's TLB, after the
/// caller has changed the page tables. Returns once all CPUs have done so.
/// Call with interrupts on and no `SpinLock` held (see the module note).
pub fn shootdown(start: u64, end: u64) {
    assert!(cpu::interrupts_enabled(), "TLB shootdown with interrupts off");
    cpu::cli();
    let me = percpu::index();
    let others = (0..smp::online()).filter(|&c| c != me).fold(0u64, |m, c| m | 1 << c);
    run(others, start, end);
    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    cpu::sti();
}

/// Flush user mappings in [start, end) of the address space `pml4` from the
/// TLB of every CPU that has it loaded (this one included), after the caller
/// changed its page tables. Same rules as `shootdown`.
pub fn shootdown_user(pml4: u64, start: u64, end: u64) {
    assert!(cpu::interrupts_enabled(), "TLB shootdown with interrupts off");
    cpu::cli();
    // Pairs with the fence in `switch_to`: either that CPU's entry shows
    // `pml4` here, or it loads the page tables after our change.
    core::sync::atomic::fence(Ordering::SeqCst);
    let me = percpu::index();
    let targets = (0..smp::online())
        .filter(|&c| c != me && ACTIVE[c].load(Ordering::SeqCst) == pml4)
        .fold(0u64, |m, c| m | 1 << c);
    if targets == 0 {
        if ACTIVE[me].load(Ordering::Relaxed) == pml4 {
            flush_local(start, end);
        }
    } else {
        run(targets, start, end);
        USER_SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    }
    cpu::sti();
}

/// Flush [start, end) here and on the CPUs in `targets` (a bit per CPU
/// index). Interrupts off.
fn run(targets: u64, start: u64, end: u64) {
    while BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        catch_up(); // the CPU holding BUSY may be waiting for us
        core::hint::spin_loop();
    }
    let me = percpu::index();
    START.store(start, Ordering::Relaxed);
    END.store(end, Ordering::Relaxed);
    let generation = GENERATION.load(Ordering::Relaxed) + 1;
    GENERATION.store(generation, Ordering::Release);
    flush_local(start, end);
    ACKED[me].store(generation, Ordering::Relaxed);

    let cpus = || (0..smp::online()).filter(move |&c| targets & 1 << c != 0);
    for cpu in cpus() {
        apic::send_ipi(smp::lapic_id_of(cpu), VECTOR_TLB_SHOOTDOWN as u8);
    }
    for cpu in cpus() {
        let mut spins: u64 = 0;
        while ACKED[cpu].load(Ordering::Acquire) != generation {
            spins += 1;
            if spins == 4_000_000_000 {
                panic!("TLB shootdown: CPU {} doesn't answer", cpu);
            }
            core::hint::spin_loop();
        }
    }
    BUSY.store(false, Ordering::Release);
}

/// Kernel shootdowns, the flushes other CPUs did for shootdowns, and user
/// shootdowns that had to interrupt another CPU.
pub fn stats() -> (u64, u64, u64) {
    (
        SHOOTDOWNS.load(Ordering::Relaxed),
        REMOTE_FLUSHES.load(Ordering::Relaxed),
        USER_SHOOTDOWNS.load(Ordering::Relaxed),
    )
}
