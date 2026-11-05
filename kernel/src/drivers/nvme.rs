//! NVMe SSDs (PCI class 01:08:02).
//!
//! The simplest correct driver: one admin queue pair and one I/O queue
//! pair, one command in flight at a time (behind a `Mutex`), completions
//! polled rather than interrupt-driven, and a single page as the DMA bounce
//! buffer, so every command moves one 4 KiB block with one PRP entry. NVMe
//! completes in microseconds, so polling costs little; a faster driver would
//! use MSI-X and several commands in flight.

use super::block::{Block, BlockDevice, IoError, BLOCK_SIZE};
use super::pci::Function;
use crate::mm::{phys_to_virt, pmm, vmm, PAGE_SIZE};
use crate::sync::Mutex;
use alloc::{format, string::String, sync::Arc};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicUsize, Ordering};

// Controller registers (offsets into BAR 0).
const CAP: usize = 0x00;
const CC: usize = 0x14;
const CSTS: usize = 0x1c;
const AQA: usize = 0x24;
const ASQ: usize = 0x28;
const ACQ: usize = 0x30;
const DOORBELLS: usize = 0x1000;

const CC_ENABLE: u32 = 1;
const CC_IO_QUEUE_SIZES: u32 = 6 << 16 | 4 << 20; // 64-byte commands, 16-byte completions
const CSTS_READY: u32 = 1;
const CSTS_FATAL: u32 = 1 << 1;

// Opcodes.
const ADMIN_CREATE_IO_SQ: u32 = 0x01;
const ADMIN_CREATE_IO_CQ: u32 = 0x05;
const ADMIN_IDENTIFY: u32 = 0x06;
const IO_WRITE: u32 = 0x01;
const IO_READ: u32 = 0x02;

/// Queue length (entries), if the controller allows that many.
const QUEUE_ENTRIES: u32 = 64;
/// How long to poll before giving up on the controller (in spins: this also
/// runs during boot, with interrupts off, when uptime doesn't advance).
const SPIN_LIMIT: u64 = 2_000_000_000;

static NEXT_INDEX: AtomicUsize = AtomicUsize::new(0);

/// A submission queue and its completion queue, one page each.
struct Queue {
    id: u16,
    entries: u32,
    sq: u64, // physical addresses
    cq: u64,
    sq_tail: u32,
    cq_head: u32,
    /// Phase bit that marks a new completion entry; flips at each wrap.
    phase: u32,
}

struct Controller {
    regs: u64, // virtual address of BAR 0
    doorbell_stride: usize,
    admin: Queue,
    io: Option<Queue>,
    /// One page for every data transfer.
    buffer: u64,
}

pub struct Nvme {
    name: String,
    namespace: u32,
    /// Device sectors per 4 KiB block.
    sectors_per_block: u64,
    blocks: u64,
    controller: Mutex<Controller>,
}

impl Controller {
    fn read32(&self, reg: usize) -> u32 {
        unsafe { read_volatile((self.regs + reg as u64) as *const u32) }
    }

    fn write32(&self, reg: usize, value: u32) {
        unsafe { write_volatile((self.regs + reg as u64) as *mut u32, value) }
    }

    fn read64(&self, reg: usize) -> u64 {
        self.read32(reg) as u64 | (self.read32(reg + 4) as u64) << 32
    }

    fn write64(&self, reg: usize, value: u64) {
        self.write32(reg, value as u32);
        self.write32(reg + 4, (value >> 32) as u32);
    }

    fn wait_status(&self, ready: bool) -> Result<(), IoError> {
        for _ in 0..SPIN_LIMIT {
            let status = self.read32(CSTS);
            if status & CSTS_FATAL != 0 {
                return Err(IoError);
            }
            if (status & CSTS_READY != 0) == ready {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(IoError)
    }

    /// Submit one command on the admin queue (`io` false) or the I/O queue,
    /// and poll for its completion. Returns the completion's dword 0.
    fn submit(&mut self, io: bool, mut command: [u32; 16]) -> Result<u32, IoError> {
        let stride = self.doorbell_stride;
        let regs = self.regs;
        let queue = if io { self.io.as_mut().ok_or(IoError)? } else { &mut self.admin };
        command[0] |= (queue.sq_tail) << 16; // command id: the slot is unique while in flight
        let slot = phys_to_virt(queue.sq) + queue.sq_tail as u64 * 64;
        for (i, dword) in command.iter().enumerate() {
            unsafe { write_volatile((slot as *mut u32).add(i), *dword) };
        }
        queue.sq_tail = (queue.sq_tail + 1) % queue.entries;
        fence(Ordering::SeqCst); // the command is in memory before the doorbell rings
        let sq_doorbell = regs + (DOORBELLS + 2 * queue.id as usize * stride) as u64;
        unsafe { write_volatile(sq_doorbell as *mut u32, queue.sq_tail) };

        let entry = phys_to_virt(queue.cq) + queue.cq_head as u64 * 16;
        let mut done = None;
        for _ in 0..SPIN_LIMIT {
            let dw3 = unsafe { read_volatile((entry + 12) as *const u32) };
            if (dw3 >> 16) & 1 == queue.phase {
                done = Some(dw3);
                break;
            }
            core::hint::spin_loop();
        }
        let dw3 = done.ok_or(IoError)?;
        fence(Ordering::SeqCst);
        let dw0 = unsafe { read_volatile(entry as *const u32) };
        queue.cq_head += 1;
        if queue.cq_head == queue.entries {
            queue.cq_head = 0;
            queue.phase ^= 1;
        }
        let cq_doorbell = regs + (DOORBELLS + (2 * queue.id as usize + 1) * stride) as u64;
        unsafe { write_volatile(cq_doorbell as *mut u32, queue.cq_head) };
        if dw3 >> 17 != 0 {
            return Err(IoError); // status code: the command failed
        }
        Ok(dw0)
    }

    /// Identify (CNS `cns`) into the buffer page.
    fn identify(&mut self, cns: u32, namespace: u32) -> Result<(), IoError> {
        let mut command = [0u32; 16];
        command[0] = ADMIN_IDENTIFY;
        command[1] = namespace;
        command[6] = self.buffer as u32;
        command[7] = (self.buffer >> 32) as u32;
        command[10] = cns;
        self.submit(false, command).map(|_| ())
    }

    fn buffer(&self) -> &mut [u8; PAGE_SIZE as usize] {
        unsafe { &mut *(phys_to_virt(self.buffer) as *mut [u8; PAGE_SIZE as usize]) }
    }
}

fn new_queue(id: u16, entries: u32) -> Option<Queue> {
    Some(Queue {
        id,
        entries,
        sq: pmm::alloc_zeroed_frame()?,
        cq: pmm::alloc_zeroed_frame()?,
        sq_tail: 0,
        cq_head: 0,
        phase: 1,
    })
}

/// Set up the controller behind `function` and its first namespace.
pub fn probe(function: &Function) -> Result<Arc<Nvme>, &'static str> {
    let bar = function.memory_bar(0).ok_or("no memory BAR")?;
    function.enable_memory_and_dma();
    let regs = vmm::map_mmio(bar, 0x2000);

    let mut c = Controller {
        regs,
        doorbell_stride: 0,
        admin: new_queue(0, 0).ok_or("out of memory")?,
        io: None,
        buffer: pmm::alloc_zeroed_frame().ok_or("out of memory")?,
    };
    let cap = c.read64(CAP);
    let max_entries = (cap & 0xffff) as u32 + 1;
    let entries = QUEUE_ENTRIES.min(max_entries);
    c.admin.entries = entries;
    c.doorbell_stride = 4 << ((cap >> 32) & 0xf);
    if (cap >> 37) & 1 == 0 {
        return Err("no NVM command set");
    }
    if (cap >> 48) & 0xf != 0 {
        return Err("controller can't use 4 KiB pages");
    }

    // Reset, point the controller at the admin queues, enable it.
    c.write32(CC, c.read32(CC) & !CC_ENABLE);
    c.wait_status(false).map_err(|_| "controller won't stop")?;
    c.write32(AQA, (entries - 1) << 16 | (entries - 1));
    c.write64(ASQ, c.admin.sq);
    c.write64(ACQ, c.admin.cq);
    c.write32(CC, CC_ENABLE | CC_IO_QUEUE_SIZES); // NVM command set, 4 KiB pages
    c.wait_status(true).map_err(|_| "controller won't start")?;

    // The first active namespace, and its sector size.
    c.identify(2, 0).map_err(|_| "identify failed")?;
    let namespace = u32::from_le_bytes(c.buffer()[..4].try_into().unwrap());
    if namespace == 0 {
        return Err("no namespace");
    }
    c.identify(0, namespace).map_err(|_| "identify namespace failed")?;
    let page = c.buffer();
    let sectors = u64::from_le_bytes(page[..8].try_into().unwrap());
    let format = (page[26] & 0xf) as usize;
    let lba_format = u32::from_le_bytes(page[128 + 4 * format..132 + 4 * format].try_into().unwrap());
    let sector_size = 1usize << ((lba_format >> 16) & 0xff);
    if sector_size > BLOCK_SIZE || BLOCK_SIZE % sector_size != 0 {
        return Err("unsupported sector size");
    }

    // One I/O queue pair: completion queue first, then the submission queue.
    let io = new_queue(1, entries).ok_or("out of memory")?;
    let size = (entries - 1) << 16 | 1;
    let mut create = [0u32; 16];
    create[0] = ADMIN_CREATE_IO_CQ;
    create[6] = io.cq as u32;
    create[7] = (io.cq >> 32) as u32;
    create[10] = size;
    create[11] = 1; // physically contiguous, no interrupts
    c.submit(false, create).map_err(|_| "creating the completion queue failed")?;
    create[0] = ADMIN_CREATE_IO_SQ;
    create[6] = io.sq as u32;
    create[7] = (io.sq >> 32) as u32;
    create[11] = 1 << 16 | 1; // completion queue 1, physically contiguous
    c.submit(false, create).map_err(|_| "creating the submission queue failed")?;
    c.io = Some(io);

    let sectors_per_block = (BLOCK_SIZE / sector_size) as u64;
    let index = NEXT_INDEX.fetch_add(1, Ordering::Relaxed);
    Ok(Arc::new(Nvme {
        name: format!("nvme{}n{}", index, namespace),
        namespace,
        sectors_per_block,
        blocks: sectors / sectors_per_block,
        controller: Mutex::new(c),
    }))
}

impl Nvme {
    /// Read or write block `index` through the buffer page.
    fn command(&self, c: &mut Controller, opcode: u32, index: u64) -> Result<(), IoError> {
        if index >= self.blocks {
            return Err(IoError);
        }
        let lba = index * self.sectors_per_block;
        let mut command = [0u32; 16];
        command[0] = opcode;
        command[1] = self.namespace;
        command[6] = c.buffer as u32;
        command[7] = (c.buffer >> 32) as u32;
        command[10] = lba as u32;
        command[11] = (lba >> 32) as u32;
        command[12] = (self.sectors_per_block - 1) as u32; // 0-based count
        c.submit(true, command).map(|_| ())
    }
}

impl BlockDevice for Nvme {
    fn name(&self) -> &str {
        &self.name
    }

    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_block(&self, index: u64, buf: &mut Block) -> Result<(), IoError> {
        let mut c = self.controller.lock();
        self.command(&mut c, IO_READ, index)?;
        buf.copy_from_slice(c.buffer());
        Ok(())
    }

    fn write_block(&self, index: u64, buf: &Block) -> Result<(), IoError> {
        let mut c = self.controller.lock();
        c.buffer().copy_from_slice(buf);
        self.command(&mut c, IO_WRITE, index)
    }
}
