//! In-memory files and directories. Contents live in the kernel heap and
//! are gone at reboot.

use super::{FsError, FsStats, Inode, Kind, Result};
use crate::sync::Mutex;
use alloc::{collections::BTreeMap, string::String, sync::Arc, vec::Vec};
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

pub struct File {
    data: Mutex<Vec<u8>>,
    mtime: AtomicU64,
}

/// Current time in Unix seconds (0 if the clock is unknown).
pub fn now() -> u64 {
    crate::drivers::rtc::now_ms(crate::task::uptime_ms()) / 1000
}

/// tmpfs lives in RAM: report physical memory, in pages.
fn memory_stats() -> FsStats {
    let (free, total) = crate::mm::pmm::stats();
    let page = crate::mm::PAGE_SIZE;
    FsStats { block_size: page, total_blocks: total / page, free_blocks: free / page, total_inodes: 0, free_inodes: 0 }
}

impl Inode for File {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn kind(&self) -> Kind {
        Kind::File
    }

    fn size(&self) -> u64 {
        self.data.lock().len() as u64
    }

    fn mtime(&self) -> u64 {
        self.mtime.load(Ordering::Relaxed)
    }

    fn fs_stats(&self) -> Option<FsStats> {
        Some(memory_stats())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let data = self.data.lock();
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n)
    }

    /// Writing past the end fills the gap with zeros.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        let mut data = self.data.lock();
        let end = (offset as usize).checked_add(buf.len()).ok_or(FsError::Invalid)?;
        if end > data.len() {
            let more = end - data.len();
            data.try_reserve(more).map_err(|_| FsError::NoMemory)?;
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(buf);
        self.mtime.store(now(), Ordering::Relaxed);
        Ok(buf.len())
    }

    fn truncate(&self) -> Result<()> {
        let mut data = self.data.lock();
        data.clear();
        data.shrink_to_fit();
        self.mtime.store(now(), Ordering::Relaxed);
        Ok(())
    }
}

pub struct Dir {
    entries: Mutex<BTreeMap<String, Arc<dyn Inode>>>,
}

impl Dir {
    pub fn new() -> Dir {
        Dir { entries: Mutex::new(BTreeMap::new()) }
    }

    /// Put an existing inode (a device, another directory) under `name`.
    pub fn attach(&self, name: &str, inode: Arc<dyn Inode>) -> Result<()> {
        let mut entries = self.entries.lock();
        if entries.contains_key(name) {
            return Err(FsError::Exists);
        }
        entries.insert(String::from(name), inode);
        Ok(())
    }
}

impl Inode for Dir {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn kind(&self) -> Kind {
        Kind::Dir
    }

    fn size(&self) -> u64 {
        self.entries.lock().len() as u64
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        self.entries.lock().get(name).cloned().ok_or(FsError::NotFound)
    }

    fn create(&self, name: &str, kind: Kind) -> Result<Arc<dyn Inode>> {
        let inode: Arc<dyn Inode> = match kind {
            Kind::File => Arc::new(File { data: Mutex::new(Vec::new()), mtime: AtomicU64::new(now()) }),
            Kind::Dir => Arc::new(Dir::new()),
            Kind::Device | Kind::Pipe => return Err(FsError::Invalid),
        };
        self.attach(name, inode.clone())?;
        Ok(inode)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        let mut entries = self.entries.lock();
        let inode = entries.get(name).ok_or(FsError::NotFound)?;
        if inode.kind() == Kind::Dir && !inode.is_empty() {
            return Err(FsError::NotEmpty);
        }
        // Open files keep the inode alive until they are closed.
        entries.remove(name);
        Ok(())
    }

    fn entry(&self, index: usize) -> Result<Option<(String, Kind)>> {
        Ok(self.entries.lock().iter().nth(index).map(|(name, inode)| (name.clone(), inode.kind())))
    }

    fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    fn fs_stats(&self) -> Option<FsStats> {
        Some(memory_stats())
    }

    fn rename(&self, old: &str, new_dir: &Arc<dyn Inode>, new: &str) -> Result<()> {
        let target = new_dir.as_any().downcast_ref::<Dir>().ok_or(FsError::CrossDevice)?;
        if core::ptr::eq(self, target) {
            let mut entries = self.entries.lock();
            move_entry(&mut entries, old, None, new)
        } else {
            // Two directories: lock them in address order, so two renames in
            // opposite directions can't deadlock.
            let (first, second) = if (self as *const Dir) < (target as *const Dir) { (self, target) } else { (target, self) };
            let mut a = first.entries.lock();
            let mut b = second.entries.lock();
            let (source, dest) = if core::ptr::eq(first, self) { (&mut *a, &mut *b) } else { (&mut *b, &mut *a) };
            move_entry(source, old, Some(dest), new)
        }
    }
}

type Entries = BTreeMap<String, Arc<dyn Inode>>;

/// Move `old` from `source` to `new` in `dest` (or in `source` itself if
/// `dest` is None), replacing what is there if allowed.
fn move_entry(source: &mut Entries, old: &str, dest: Option<&mut Entries>, new: &str) -> Result<()> {
    let moving = source.get(old).ok_or(FsError::NotFound)?.clone();
    let dest_map = match dest {
        Some(d) => d,
        None => {
            if let Some(existing) = source.get(new) {
                super::check_replace(moving.kind(), existing)?;
            }
            source.remove(old);
            source.insert(String::from(new), moving);
            return Ok(());
        }
    };
    if let Some(existing) = dest_map.get(new) {
        super::check_replace(moving.kind(), existing)?;
    }
    source.remove(old);
    dest_map.insert(String::from(new), moving);
    Ok(())
}
