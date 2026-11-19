//! Kernel stacks for tasks. Each stack sits in its own 64 KiB slot with the
//! bottom of the slot left unmapped, so a stack overflow hits a guard page and
//! causes a clean double fault instead of silently corrupting memory.
//!
//! Freed stacks go back to a pool with their pages still mapped, since a
//! task is often started soon after another one ends. `trim`, run by idle
//! CPUs, gives back whatever the pool holds beyond `KEEP`: it unmaps those
//! stacks and has every CPU flush them from its TLB (`tlb::shootdown`)
//! before freeing the frames, because a CPU that ran the task may still
//! cache the old translations and would otherwise write through them into
//! frames that now belong to someone else.

use super::{pmm, vmm, PAGE_SIZE};
use crate::arch::{cpu, tlb};
use crate::sync::SpinLock;
use alloc::vec::Vec;

const BASE: u64 = 0xffff_e000_0000_0000;
const SLOT_SIZE: u64 = 64 * 1024;
pub const STACK_SIZE: u64 = 32 * 1024;
const PAGES: usize = (STACK_SIZE / PAGE_SIZE) as usize;
/// Unused stacks kept mapped, ready for the next task.
const KEEP: usize = 8;

struct Slots {
    next: u64,       // first slot never used
    free: Vec<u64>,  // slots whose stack is mapped and unused
    empty: Vec<u64>, // slots with nothing mapped, to be reused
}

static SLOTS: SpinLock<Slots> = SpinLock::new(Slots { next: 0, free: Vec::new(), empty: Vec::new() });

pub struct KernelStack {
    slot: u64,
}

impl KernelStack {
    pub fn new() -> Option<Self> {
        let fresh = {
            let mut slots = SLOTS.lock();
            if let Some(slot) = slots.free.pop() {
                return Some(KernelStack { slot });
            }
            slots.empty.pop().unwrap_or_else(|| {
                slots.next += 1;
                slots.next - 1
            })
        };
        let top = slot_top(fresh);
        let mut page = top - STACK_SIZE;
        while page < top {
            let mapped = pmm::alloc_frame().and_then(|frame| {
                let f = vmm::flags::WRITABLE | vmm::flags::NO_EXECUTE | vmm::flags::GLOBAL;
                vmm::map(vmm::kernel_pml4(), page, frame, f).map_err(|_| pmm::free_frame(frame)).ok()
            });
            if mapped.is_none() {
                // Give back what was mapped so far. No other CPU has used
                // these pages, so there is nothing to shoot down.
                let mut p = top - STACK_SIZE;
                while p < page {
                    pmm::free_frame(vmm::unmap(vmm::kernel_pml4(), p).expect("stack page vanished"));
                    p += PAGE_SIZE;
                }
                SLOTS.lock().empty.push(fresh);
                return None;
            }
            page += PAGE_SIZE;
        }
        Some(KernelStack { slot: fresh })
    }

    pub fn top(&self) -> u64 {
        slot_top(self.slot)
    }
}

fn slot_top(slot: u64) -> u64 {
    BASE + (slot + 1) * SLOT_SIZE
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        SLOTS.lock().free.push(self.slot);
    }
}

/// Free the pool's stacks beyond `KEEP`. Needs interrupts on and no locks
/// held (for the shootdown); otherwise it does nothing.
pub fn trim() {
    if !cpu::interrupts_enabled() {
        return;
    }
    loop {
        let slot = {
            let mut slots = SLOTS.lock();
            if slots.free.len() <= KEEP {
                return;
            }
            slots.free.pop().unwrap()
        };
        let top = slot_top(slot);
        let mut frames = [0u64; PAGES];
        for (i, frame) in frames.iter_mut().enumerate() {
            let page = top - STACK_SIZE + i as u64 * PAGE_SIZE;
            *frame = vmm::unmap(vmm::kernel_pml4(), page).expect("stack page vanished");
        }
        tlb::shootdown(top - STACK_SIZE, top);
        for frame in frames {
            pmm::free_frame(frame);
        }
        SLOTS.lock().empty.push(slot);
    }
}

/// Kernel stacks mapped now: those in use plus the pool.
pub fn mapped() -> u64 {
    let slots = SLOTS.lock();
    slots.next - slots.empty.len() as u64
}
