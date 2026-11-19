//! TLB shootdown: making every CPU forget a kernel mapping that changed.
//!
//! Each CPU caches page translations in its TLB, and kernel mappings are
//! global (they survive CR3 switches), so after the kernel unmaps one, any
//! CPU that used it may still translate through the old entry. Before the
//! frame behind it can be reused, every CPU must flush it: the one making
//! the change does so itself, the others get an IPI and acknowledge.
//!
//! One shootdown runs at a time (`BUSY`). The initiator waits for the
//! acknowledgements with interrupts off, so a second initiator spinning for
//! `BUSY`, also with interrupts off, couldn't take the first one's IPI:
//! instead it handles the pending request itself while it spins. For the
//! same reason the caller must not hold a `SpinLock`: another CPU may be
//! spinning on it with interrupts off, and would never acknowledge.
//!
//! User mappings don't need this yet: a process's page tables are loaded
//! only on the CPU running it, and switching CR3 flushes its entries.

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
static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);
/// Flushes done for another CPU's request: one per other CPU per shootdown.
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

/// Flush kernel mappings in [start, end) from every CPU's TLB, after the
/// caller has changed the page tables. Returns once all CPUs have done so.
/// Call with interrupts on and no `SpinLock` held (see the module note).
pub fn shootdown(start: u64, end: u64) {
    assert!(cpu::interrupts_enabled(), "TLB shootdown with interrupts off");
    cpu::cli();
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

    let online = smp::online();
    for cpu in (0..online).filter(|&c| c != me) {
        apic::send_ipi(smp::lapic_id_of(cpu), VECTOR_TLB_SHOOTDOWN as u8);
    }
    for cpu in 0..online {
        let mut spins: u64 = 0;
        while ACKED[cpu].load(Ordering::Acquire) != generation {
            spins += 1;
            if spins == 4_000_000_000 {
                panic!("TLB shootdown: CPU {} doesn't answer", cpu);
            }
            core::hint::spin_loop();
        }
    }
    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    BUSY.store(false, Ordering::Release);
    cpu::sti();
}

/// Shootdowns since boot, and the flushes other CPUs did for them.
pub fn stats() -> (u64, u64) {
    (SHOOTDOWNS.load(Ordering::Relaxed), REMOTE_FLUSHES.load(Ordering::Relaxed))
}
