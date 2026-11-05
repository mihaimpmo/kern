//! Block devices: disks addressed in 4 KiB blocks, and the cache in front
//! of them.

use crate::sync::{Mutex, SpinLock};
use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};

pub const BLOCK_SIZE: usize = 4096;
pub type Block = [u8; BLOCK_SIZE];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IoError;

pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    /// Size in BLOCK_SIZE blocks.
    fn block_count(&self) -> u64;
    fn read_block(&self, index: u64, buf: &mut Block) -> Result<(), IoError>;
    fn write_block(&self, index: u64, buf: &Block) -> Result<(), IoError>;
}

static DEVICES: SpinLock<Vec<Arc<dyn BlockDevice>>> = SpinLock::new(Vec::new());

pub fn register(device: Arc<dyn BlockDevice>) {
    DEVICES.lock().push(device);
}

pub fn devices() -> Vec<Arc<dyn BlockDevice>> {
    DEVICES.lock().clone()
}

/// A write-through cache of recently used blocks in front of a device:
/// reads are served from memory when possible, and every write goes to the
/// device before it returns, so nothing is lost at power-off.
pub struct Cached {
    device: Arc<dyn BlockDevice>,
    cache: Mutex<Lru>,
}

struct Lru {
    blocks: BTreeMap<u64, (Box<Block>, u64)>, // index -> (data, last use)
    clock: u64,
    capacity: usize,
}

impl Lru {
    fn touch(&mut self, index: u64) -> Option<&mut Box<Block>> {
        self.clock += 1;
        let clock = self.clock;
        self.blocks.get_mut(&index).map(|(data, used)| {
            *used = clock;
            data
        })
    }

    fn insert(&mut self, index: u64, data: &Block) {
        if self.touch(index).map(|b| **b = *data).is_some() {
            return;
        }
        if self.blocks.len() >= self.capacity {
            // Evict the least recently used block. A linear scan is fine for
            // a cache of this size next to the cost of a disk access.
            if let Some(&oldest) = self.blocks.iter().min_by_key(|(_, (_, used))| *used).map(|(i, _)| i) {
                self.blocks.remove(&oldest);
            }
        }
        self.blocks.insert(index, (Box::new(*data), self.clock));
    }
}

impl Cached {
    pub fn new(device: Arc<dyn BlockDevice>, capacity: usize) -> Cached {
        Cached { device, cache: Mutex::new(Lru { blocks: BTreeMap::new(), clock: 0, capacity }) }
    }

    pub fn block_count(&self) -> u64 {
        self.device.block_count()
    }

    pub fn read(&self, index: u64, buf: &mut Block) -> Result<(), IoError> {
        let mut cache = self.cache.lock();
        if let Some(data) = cache.touch(index) {
            buf.copy_from_slice(&data[..]);
            return Ok(());
        }
        self.device.read_block(index, buf)?;
        cache.insert(index, buf);
        Ok(())
    }

    pub fn write(&self, index: u64, buf: &Block) -> Result<(), IoError> {
        let mut cache = self.cache.lock();
        match self.device.write_block(index, buf) {
            Ok(()) => {
                cache.insert(index, buf);
                Ok(())
            }
            Err(e) => {
                cache.blocks.remove(&index); // the disk's contents are unknown now
                Err(e)
            }
        }
    }
}
