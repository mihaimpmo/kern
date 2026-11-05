//! PCI enumeration through the legacy configuration ports (0xCF8/0xCFC),
//! which every x86 PC still has. That reaches the standard 256-byte config
//! space of each function, which is all the drivers here need (IDs, class,
//! BARs, the command register).

use crate::arch::cpu::{inl, outl};
use crate::sync::SpinLock;
use alloc::vec::Vec;

const CONFIG_ADDRESS: u16 = 0xcf8;
const CONFIG_DATA: u16 = 0xcfc;

/// The two ports form one register pair, so accesses must not interleave.
static CONFIG: SpinLock<()> = SpinLock::new(());

#[derive(Clone, Copy, Debug)]
pub struct Function {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

fn address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    1 << 31 | (bus as u32) << 16 | (device as u32) << 11 | (function as u32) << 8 | (offset as u32 & 0xfc)
}

fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let _lock = CONFIG.lock();
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, device, function, offset));
        inl(CONFIG_DATA)
    }
}

fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let _lock = CONFIG.lock();
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, device, function, offset));
        outl(CONFIG_DATA, value);
    }
}

impl Function {
    pub fn read32(&self, offset: u8) -> u32 {
        read32(self.bus, self.device, self.function, offset)
    }

    pub fn write32(&self, offset: u8, value: u32) {
        write32(self.bus, self.device, self.function, offset, value)
    }

    /// Physical address of memory BAR `index` (a 64-bit BAR takes two
    /// slots), or None if it is an I/O BAR or unused.
    pub fn memory_bar(&self, index: u8) -> Option<u64> {
        let low = self.read32(0x10 + index * 4);
        if low & 1 != 0 {
            return None; // I/O space
        }
        let base = (low & !0xf) as u64;
        let address = match (low >> 1) & 3 {
            0b10 => base | (self.read32(0x14 + index * 4) as u64) << 32,
            _ => base,
        };
        (address != 0).then_some(address)
    }

    /// Let the device decode its memory BARs and do DMA.
    pub fn enable_memory_and_dma(&self) {
        const MEMORY_SPACE: u32 = 1 << 1;
        const BUS_MASTER: u32 = 1 << 2;
        const INTX_DISABLE: u32 = 1 << 10; // the drivers here poll
        let command = self.read32(0x04);
        self.write32(0x04, (command & 0xffff) | MEMORY_SPACE | BUS_MASTER | INTX_DISABLE);
    }
}

fn probe(bus: u8, device: u8, function: u8) -> Option<Function> {
    let id = read32(bus, device, function, 0x00);
    if id & 0xffff == 0xffff {
        return None; // nothing there
    }
    let class = read32(bus, device, function, 0x08);
    Some(Function {
        bus,
        device,
        function,
        vendor: id as u16,
        device_id: (id >> 16) as u16,
        class: (class >> 24) as u8,
        subclass: (class >> 16) as u8,
        prog_if: (class >> 8) as u8,
    })
}

/// Every function on every bus.
pub fn enumerate() -> Vec<Function> {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let Some(first) = probe(bus, device, 0) else { continue };
            found.push(first);
            let header_type = (read32(bus, device, 0, 0x0c) >> 16) as u8;
            if header_type & 0x80 != 0 {
                found.extend((1..8).filter_map(|f| probe(bus, device, f)));
            }
        }
    }
    found
}
