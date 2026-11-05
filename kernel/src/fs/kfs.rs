//! KFS, Kern's on-disk filesystem.
//!
//! The format is described in full in tools/kfs.py, the host tool that
//! creates (`mkfs`), inspects and checks images; the two must agree. In
//! short: 4 KiB blocks; block 0 is the superblock, then the inode bitmap,
//! the block bitmap, the inode table (128-byte inodes with 12 direct, one
//! indirect and one double-indirect block pointer) and the data blocks.
//! Directories are files of 256-byte entries (inode, kind, name); there are
//! no "." or ".." entries, since the VFS resolves those textually.
//!
//! All operations on one filesystem take its `Mutex`, which also covers the
//! disk I/O. Writes go straight to the disk (the block cache is
//! write-through), in an order that can at worst leak blocks on a crash:
//! allocate (bitmap) first, then write data, then the inode that points to
//! it. There is no journal and no in-kernel fsck; `tools/kfs.py check`
//! verifies an image.
//!
//! The kernel never formats a disk: it only mounts one whose superblock
//! already has the KFS magic, so other disks (an OS on a laptop's SSD, say)
//! are never written to.

use super::{tmpfs, FsError, FsStats, Inode, Kind, Result};
use crate::drivers::block::{Block, BlockDevice, Cached, IoError, BLOCK_SIZE};
use crate::sync::Mutex;
use alloc::{collections::BTreeMap, string::String, sync::Arc, sync::Weak};
use core::any::Any;

const MAGIC: &[u8; 8] = b"KernFS01";
const INODE_SIZE: usize = 128;
const INODES_PER_BLOCK: u64 = (BLOCK_SIZE / INODE_SIZE) as u64;
const ENTRY_SIZE: usize = 256;
const ENTRIES_PER_BLOCK: usize = BLOCK_SIZE / ENTRY_SIZE;
pub const MAX_NAME: usize = 248;
const DIRECT: u64 = 12;
const POINTERS: u64 = (BLOCK_SIZE / 4) as u64;
const MAX_BLOCKS: u64 = DIRECT + POINTERS + POINTERS * POINTERS;
const BITS_PER_BLOCK: u64 = BLOCK_SIZE as u64 * 8;
const KIND_FREE: u16 = 0;
const KIND_FILE: u16 = 1;
const KIND_DIR: u16 = 2;
const ROOT: u32 = 1;
/// Blocks kept in memory (4 MiB).
const CACHE_BLOCKS: usize = 1024;

impl From<IoError> for FsError {
    fn from(_: IoError) -> FsError {
        FsError::Io
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

struct Superblock {
    total_blocks: u64,
    inode_count: u64,
    inode_bitmap_start: u64,
    inode_bitmap_blocks: u64,
    block_bitmap_start: u64,
    block_bitmap_blocks: u64,
    inode_table_start: u64,
    inode_table_blocks: u64,
    data_start: u64,
}

impl Superblock {
    fn parse(b: &Block) -> core::result::Result<Superblock, &'static str> {
        if &b[..8] != MAGIC {
            return Err("no Kern filesystem");
        }
        if u32_at(b, 8) as usize != BLOCK_SIZE || u32_at(b, 12) as usize != INODE_SIZE {
            return Err("unsupported block or inode size");
        }
        let f = |i: usize| u64_at(b, 16 + 8 * i);
        let sb = Superblock {
            total_blocks: f(0),
            inode_count: f(1),
            inode_bitmap_start: f(2),
            inode_bitmap_blocks: f(3),
            block_bitmap_start: f(4),
            block_bitmap_blocks: f(5),
            inode_table_start: f(6),
            inode_table_blocks: f(7),
            data_start: f(8),
        };
        let fits = |start: u64, len: u64| start.checked_add(len).map_or(false, |end| end <= sb.data_start);
        let sane = f(9) == ROOT as u64
            && sb.inode_count > ROOT as u64
            && sb.inode_bitmap_blocks * BITS_PER_BLOCK >= sb.inode_count
            && sb.block_bitmap_blocks * BITS_PER_BLOCK >= sb.total_blocks
            && sb.inode_table_blocks * INODES_PER_BLOCK >= sb.inode_count
            && fits(sb.inode_bitmap_start, sb.inode_bitmap_blocks)
            && fits(sb.block_bitmap_start, sb.block_bitmap_blocks)
            && fits(sb.inode_table_start, sb.inode_table_blocks)
            && sb.inode_bitmap_start >= 1
            && sb.data_start < sb.total_blocks
            && sb.total_blocks <= u32::MAX as u64;
        if sane { Ok(sb) } else { Err("corrupt superblock") }
    }
}

/// An inode as stored on disk.
#[derive(Clone, Copy)]
struct DiskInode {
    kind: u16,
    links: u16,
    size: u64,
    mtime: u64,
    direct: [u32; DIRECT as usize],
    indirect: u32,
    double: u32,
}

impl DiskInode {
    fn new(kind: u16) -> DiskInode {
        DiskInode { kind, links: 1, size: 0, mtime: tmpfs::now(), direct: [0; DIRECT as usize], indirect: 0, double: 0 }
    }

    fn decode(b: &[u8]) -> DiskInode {
        let mut direct = [0; DIRECT as usize];
        for (i, d) in direct.iter_mut().enumerate() {
            *d = u32_at(b, 24 + 4 * i);
        }
        DiskInode {
            kind: u16::from_le_bytes([b[0], b[1]]),
            links: u16::from_le_bytes([b[2], b[3]]),
            size: u64_at(b, 8),
            mtime: u64_at(b, 16),
            direct,
            indirect: u32_at(b, 72),
            double: u32_at(b, 76),
        }
    }

    fn encode(&self, b: &mut [u8]) {
        b[..INODE_SIZE].fill(0);
        b[0..2].copy_from_slice(&self.kind.to_le_bytes());
        b[2..4].copy_from_slice(&self.links.to_le_bytes());
        b[8..16].copy_from_slice(&self.size.to_le_bytes());
        b[16..24].copy_from_slice(&self.mtime.to_le_bytes());
        for (i, d) in self.direct.iter().enumerate() {
            b[24 + 4 * i..28 + 4 * i].copy_from_slice(&d.to_le_bytes());
        }
        b[72..76].copy_from_slice(&self.indirect.to_le_bytes());
        b[76..80].copy_from_slice(&self.double.to_le_bytes());
    }
}

/// A used directory entry: where it is, and what it names.
#[derive(Clone)]
struct Entry {
    slot: u64,
    ino: u32,
    kind: u16,
    name: String,
}

fn kind_of(code: u16) -> Kind {
    if code == KIND_DIR { Kind::Dir } else { Kind::File }
}

pub struct Kfs {
    disk: Cached,
    sb: Superblock,
    state: Mutex<State>,
}

struct State {
    free_blocks: u64,
    free_inodes: u64,
    block_hint: u64,
    inode_hint: u64,
    /// The `Node` objects handed out, so each inode has at most one and an
    /// unlinked file is freed only once nothing uses it any more.
    live: BTreeMap<u32, Weak<Node>>,
}

/// A file or directory on a KFS disk, as the VFS sees it.
pub struct Node {
    fs: Arc<Kfs>,
    ino: u32,
    kind: Kind,
}

/// Mount the KFS on `device`; returns its root directory. Fails, without
/// writing anything, if the disk doesn't hold a valid KFS.
pub fn mount(device: Arc<dyn BlockDevice>) -> core::result::Result<Arc<Node>, &'static str> {
    let disk = Cached::new(device, CACHE_BLOCKS);
    let mut block = [0u8; BLOCK_SIZE];
    disk.read(0, &mut block).map_err(|_| "cannot read the superblock")?;
    let sb = Superblock::parse(&block)?;
    if sb.total_blocks > disk.block_count() {
        return Err("filesystem is larger than the disk");
    }
    let fs = Kfs {
        disk,
        sb,
        state: Mutex::new(State { free_blocks: 0, free_inodes: 0, block_hint: 0, inode_hint: 0, live: BTreeMap::new() }),
    };
    let free_blocks = fs.count_free(fs.sb.block_bitmap_start, fs.sb.total_blocks).map_err(|_| "cannot read the bitmaps")?;
    let free_inodes = fs.count_free(fs.sb.inode_bitmap_start, fs.sb.inode_count).map_err(|_| "cannot read the bitmaps")?;
    if fs.read_inode(ROOT).map_err(|_| "cannot read the root")?.kind != KIND_DIR {
        return Err("root is not a directory");
    }
    let fs = Arc::new(fs);
    let mut st = fs.state.lock();
    st.free_blocks = free_blocks;
    st.free_inodes = free_inodes;
    st.block_hint = fs.sb.data_start;
    st.inode_hint = ROOT as u64 + 1;
    let root = fs.node(&mut st, ROOT, Kind::Dir);
    drop(st);
    Ok(root)
}

impl Kfs {
    // ------------------------------------------------------------ bitmaps

    fn count_free(&self, start: u64, count: u64) -> Result<u64> {
        let mut block = [0u8; BLOCK_SIZE];
        let mut free = 0;
        for n in 0..count.div_ceil(BITS_PER_BLOCK) {
            self.disk.read(start + n, &mut block)?;
            let bits = (count - n * BITS_PER_BLOCK).min(BITS_PER_BLOCK);
            free += (0..bits).filter(|&i| block[i as usize / 8] >> (i % 8) & 1 == 0).count() as u64;
        }
        Ok(free)
    }

    /// Find a clear bit in [first, count), starting near `hint`; set it.
    fn alloc_bit(&self, start: u64, count: u64, first: u64, hint: &mut u64) -> Result<u64> {
        let blocks = count.div_ceil(BITS_PER_BLOCK);
        let mut block = [0u8; BLOCK_SIZE];
        for step in 0..blocks {
            let b = (*hint / BITS_PER_BLOCK + step) % blocks;
            self.disk.read(start + b, &mut block)?;
            for (i, byte) in block.iter_mut().enumerate().filter(|(_, byte)| **byte != 0xff) {
                for bit in 0..8 {
                    let n = b * BITS_PER_BLOCK + i as u64 * 8 + bit;
                    if n >= first && n < count && *byte & (1 << bit) == 0 {
                        *byte |= 1 << bit;
                        self.disk.write(start + b, &block)?;
                        *hint = n + 1;
                        return Ok(n);
                    }
                }
            }
        }
        Err(FsError::NoSpace)
    }

    fn clear_bit(&self, start: u64, n: u64) -> Result<()> {
        let mut block = [0u8; BLOCK_SIZE];
        let b = start + n / BITS_PER_BLOCK;
        self.disk.read(b, &mut block)?;
        let i = (n % BITS_PER_BLOCK) as usize;
        block[i / 8] &= !(1 << (i % 8));
        self.disk.write(b, &block)?;
        Ok(())
    }

    /// A new block. Zero it unless the caller is about to fill it anyway.
    fn alloc_block(&self, st: &mut State, zero: bool) -> Result<u32> {
        let sb = &self.sb;
        let n = self.alloc_bit(sb.block_bitmap_start, sb.total_blocks, sb.data_start, &mut st.block_hint)?;
        st.free_blocks -= 1;
        if zero {
            self.disk.write(n, &[0; BLOCK_SIZE])?;
        }
        Ok(n as u32)
    }

    fn free_block(&self, st: &mut State, n: u32) -> Result<()> {
        self.clear_bit(self.sb.block_bitmap_start, n as u64)?;
        st.free_blocks += 1;
        Ok(())
    }

    // ------------------------------------------------------------- inodes

    fn read_inode(&self, ino: u32) -> Result<DiskInode> {
        let mut block = [0u8; BLOCK_SIZE];
        self.disk.read(self.sb.inode_table_start + ino as u64 / INODES_PER_BLOCK, &mut block)?;
        let at = (ino as u64 % INODES_PER_BLOCK) as usize * INODE_SIZE;
        Ok(DiskInode::decode(&block[at..at + INODE_SIZE]))
    }

    fn write_inode(&self, ino: u32, inode: &DiskInode) -> Result<()> {
        let mut block = [0u8; BLOCK_SIZE];
        let b = self.sb.inode_table_start + ino as u64 / INODES_PER_BLOCK;
        self.disk.read(b, &mut block)?;
        let at = (ino as u64 % INODES_PER_BLOCK) as usize * INODE_SIZE;
        inode.encode(&mut block[at..at + INODE_SIZE]);
        self.disk.write(b, &block)?;
        Ok(())
    }

    fn alloc_inode(&self, st: &mut State, kind: u16) -> Result<u32> {
        let sb = &self.sb;
        let n = self.alloc_bit(sb.inode_bitmap_start, sb.inode_count, ROOT as u64 + 1, &mut st.inode_hint)? as u32;
        st.free_inodes -= 1;
        self.write_inode(n, &DiskInode::new(kind))?;
        Ok(n)
    }

    /// Release an inode nothing names or uses any more, and its blocks.
    fn free_inode(&self, st: &mut State, ino: u32) -> Result<()> {
        let mut inode = self.read_inode(ino)?;
        self.free_data(st, &mut inode)?;
        inode.kind = KIND_FREE;
        self.write_inode(ino, &inode)?;
        self.clear_bit(self.sb.inode_bitmap_start, ino as u64)?;
        st.free_inodes += 1;
        Ok(())
    }

    /// The `Node` for `ino`: the one already handed out, or a new one.
    fn node(self: &Arc<Self>, st: &mut State, ino: u32, kind: Kind) -> Arc<Node> {
        if let Some(node) = st.live.get(&ino).and_then(Weak::upgrade) {
            return node;
        }
        let node = Arc::new(Node { fs: self.clone(), ino, kind });
        st.live.insert(ino, Arc::downgrade(&node));
        node
    }

    fn in_use(st: &State, ino: u32) -> bool {
        st.live.get(&ino).map_or(false, |w| w.strong_count() > 0)
    }

    /// One fewer directory entry names `ino`; free it if that was the last
    /// and nothing has it open.
    fn drop_link(&self, st: &mut State, ino: u32) -> Result<()> {
        let mut inode = self.read_inode(ino)?;
        inode.links = inode.links.saturating_sub(1);
        self.write_inode(ino, &inode)?;
        if inode.links == 0 && !Self::in_use(st, ino) {
            self.free_inode(st, ino)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ mapping

    /// Entry `index` of the pointer table in block `table`, allocated if
    /// asked. Returns (block, newly allocated).
    fn table_slot(&self, st: &mut State, table: u32, index: u64, create: bool, zero: bool) -> Result<(u32, bool)> {
        let mut block = [0u8; BLOCK_SIZE];
        self.disk.read(table as u64, &mut block)?;
        let at = index as usize * 4;
        let current = u32_at(&block, at);
        if current != 0 || !create {
            return Ok((current, false));
        }
        let new = self.alloc_block(st, zero)?;
        block[at..at + 4].copy_from_slice(&new.to_le_bytes());
        self.disk.write(table as u64, &block)?;
        Ok((new, true))
    }

    /// The disk block holding block `index` of the file (0 = a hole). With
    /// `create`, holes are filled; data blocks then come back unzeroed, with
    /// the flag saying so, but pointer tables are always zeroed.
    fn map(&self, st: &mut State, inode: &mut DiskInode, index: u64, create: bool) -> Result<(u32, bool)> {
        if index >= MAX_BLOCKS {
            return Err(FsError::NoSpace); // larger than the format allows
        }
        if index < DIRECT {
            let slot = &mut inode.direct[index as usize];
            if *slot == 0 && create {
                *slot = self.alloc_block(st, false)?;
                return Ok((*slot, true));
            }
            return Ok((*slot, false));
        }
        let index = index - DIRECT;
        if index < POINTERS {
            if inode.indirect == 0 {
                if !create {
                    return Ok((0, false));
                }
                inode.indirect = self.alloc_block(st, true)?;
            }
            return self.table_slot(st, inode.indirect, index, create, false);
        }
        let index = index - POINTERS;
        if inode.double == 0 {
            if !create {
                return Ok((0, false));
            }
            inode.double = self.alloc_block(st, true)?;
        }
        let (middle, _) = self.table_slot(st, inode.double, index / POINTERS, create, true)?;
        if middle == 0 {
            return Ok((0, false));
        }
        self.table_slot(st, middle, index % POINTERS, create, false)
    }

    /// Free every block of the file and make it empty.
    fn free_data(&self, st: &mut State, inode: &mut DiskInode) -> Result<()> {
        for d in inode.direct.iter_mut().filter(|d| **d != 0) {
            self.free_block(st, *d)?;
            *d = 0;
        }
        let mut table = [0u8; BLOCK_SIZE];
        let mut middle = [0u8; BLOCK_SIZE];
        if inode.indirect != 0 {
            self.disk.read(inode.indirect as u64, &mut table)?;
            for i in 0..POINTERS as usize {
                match u32_at(&table, i * 4) {
                    0 => {}
                    b => self.free_block(st, b)?,
                }
            }
            self.free_block(st, inode.indirect)?;
            inode.indirect = 0;
        }
        if inode.double != 0 {
            self.disk.read(inode.double as u64, &mut table)?;
            for j in 0..POINTERS as usize {
                let mid = u32_at(&table, j * 4);
                if mid == 0 {
                    continue;
                }
                self.disk.read(mid as u64, &mut middle)?;
                for i in 0..POINTERS as usize {
                    match u32_at(&middle, i * 4) {
                        0 => {}
                        b => self.free_block(st, b)?,
                    }
                }
                self.free_block(st, mid)?;
            }
            self.free_block(st, inode.double)?;
            inode.double = 0;
        }
        inode.size = 0;
        Ok(())
    }

    // ------------------------------------------------------------ file data

    fn read_at(&self, st: &mut State, ino: u32, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let mut inode = self.read_inode(ino)?;
        if offset >= inode.size {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(inode.size - offset) as usize;
        let mut block = [0u8; BLOCK_SIZE];
        let mut done = 0;
        while done < len {
            let pos = offset + done as u64;
            let within = (pos % BLOCK_SIZE as u64) as usize;
            let take = (BLOCK_SIZE - within).min(len - done);
            match self.map(st, &mut inode, pos / BLOCK_SIZE as u64, false)?.0 {
                0 => buf[done..done + take].fill(0),
                b => {
                    self.disk.read(b as u64, &mut block)?;
                    buf[done..done + take].copy_from_slice(&block[within..within + take]);
                }
            }
            done += take;
        }
        Ok(len)
    }

    fn write_at(&self, st: &mut State, ino: u32, offset: u64, data: &[u8]) -> Result<usize> {
        let mut inode = self.read_inode(ino)?;
        offset.checked_add(data.len() as u64).ok_or(FsError::Invalid)?;
        let mut block = [0u8; BLOCK_SIZE];
        let mut done = 0;
        let mut error = None;
        while done < data.len() {
            let pos = offset + done as u64;
            let within = (pos % BLOCK_SIZE as u64) as usize;
            let take = (BLOCK_SIZE - within).min(data.len() - done);
            let (b, fresh) = match self.map(st, &mut inode, pos / BLOCK_SIZE as u64, true) {
                Ok(mapped) => mapped,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            };
            if take < BLOCK_SIZE {
                if fresh {
                    block.fill(0);
                } else if let Err(e) = self.disk.read(b as u64, &mut block) {
                    error = Some(e.into());
                    break;
                }
            }
            block[within..within + take].copy_from_slice(&data[done..done + take]);
            if let Err(e) = self.disk.write(b as u64, &block) {
                error = Some(e.into());
                break;
            }
            done += take;
        }
        // Record what was written (and every block allocated) even after an
        // error, so the inode matches the bitmap.
        inode.size = inode.size.max(offset + done as u64);
        inode.mtime = tmpfs::now();
        self.write_inode(ino, &inode)?;
        match error {
            Some(e) if done == 0 => Err(e),
            _ => Ok(done),
        }
    }

    // ------------------------------------------------------------ directories

    /// Every used entry of directory `dir`, in slot order.
    fn entries(&self, st: &mut State, dir: u32) -> Result<alloc::vec::Vec<Entry>> {
        let mut inode = self.read_inode(dir)?;
        let slots = inode.size / ENTRY_SIZE as u64;
        let mut out = alloc::vec::Vec::new();
        let mut block = [0u8; BLOCK_SIZE];
        for b in 0..slots.div_ceil(ENTRIES_PER_BLOCK as u64) {
            match self.map(st, &mut inode, b, false)?.0 {
                0 => continue,
                disk_block => self.disk.read(disk_block as u64, &mut block)?,
            }
            for i in 0..ENTRIES_PER_BLOCK {
                let slot = b * ENTRIES_PER_BLOCK as u64 + i as u64;
                if slot >= slots {
                    break;
                }
                let e = &block[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
                let ino = u32_at(e, 0);
                if ino == 0 {
                    continue;
                }
                let name_len = (e[5] as usize).min(MAX_NAME);
                let name = String::from_utf8_lossy(&e[8..8 + name_len]).into_owned();
                out.push(Entry { slot, ino, kind: e[4] as u16, name });
            }
        }
        Ok(out)
    }

    fn find(&self, st: &mut State, dir: u32, name: &str) -> Result<Option<Entry>> {
        Ok(self.entries(st, dir)?.into_iter().find(|e| e.name == name))
    }

    fn write_entry(&self, st: &mut State, dir: u32, slot: u64, ino: u32, kind: u16, name: &str) -> Result<()> {
        let mut e = [0u8; ENTRY_SIZE];
        e[0..4].copy_from_slice(&ino.to_le_bytes());
        e[4] = kind as u8;
        e[5] = name.len() as u8;
        e[8..8 + name.len()].copy_from_slice(name.as_bytes());
        match self.write_at(st, dir, slot * ENTRY_SIZE as u64, &e)? {
            ENTRY_SIZE => Ok(()),
            _ => Err(FsError::NoSpace),
        }
    }

    /// Add `name` -> `ino` to `dir`: the first free slot, or a new one.
    fn add_entry(&self, st: &mut State, dir: u32, name: &str, ino: u32, kind: u16) -> Result<()> {
        if name.len() > MAX_NAME {
            return Err(FsError::NameTooLong);
        }
        let used: alloc::collections::BTreeSet<u64> = self.entries(st, dir)?.iter().map(|e| e.slot).collect();
        let slots = self.read_inode(dir)?.size / ENTRY_SIZE as u64;
        let slot = (0..slots).find(|s| !used.contains(s)).unwrap_or(slots);
        self.write_entry(st, dir, slot, ino, kind, name)
    }

    fn clear_entry(&self, st: &mut State, dir: u32, slot: u64) -> Result<()> {
        self.write_at(st, dir, slot * ENTRY_SIZE as u64, &[0; 4]).map(|_| ())
    }
}

// ---------------------------------------------------------------- VFS side

impl Drop for Node {
    fn drop(&mut self) {
        let fs = &self.fs;
        let mut st = fs.state.lock();
        if Kfs::in_use(&st, self.ino) {
            return; // a newer Node for the same inode exists
        }
        st.live.remove(&self.ino);
        // An unlinked file is freed when its last user lets go.
        if let Ok(inode) = fs.read_inode(self.ino) {
            if inode.kind != KIND_FREE && inode.links == 0 {
                let _ = fs.free_inode(&mut st, self.ino);
            }
        }
    }
}

impl Node {
    fn dir_only(&self) -> Result<()> {
        if self.kind == Kind::Dir { Ok(()) } else { Err(FsError::NotDir) }
    }

    fn file_only(&self) -> Result<()> {
        if self.kind == Kind::File { Ok(()) } else { Err(FsError::IsDir) }
    }

}

impl Inode for Node {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    /// Bytes for a file; entries for a directory (as tmpfs reports).
    fn size(&self) -> u64 {
        let mut st = self.fs.state.lock();
        match self.kind {
            Kind::Dir => self.fs.entries(&mut st, self.ino).map_or(0, |e| e.len() as u64),
            _ => self.fs.read_inode(self.ino).map_or(0, |i| i.size),
        }
    }

    fn mtime(&self) -> u64 {
        let _st = self.fs.state.lock();
        self.fs.read_inode(self.ino).map_or(0, |i| i.mtime)
    }

    fn fs_stats(&self) -> Option<FsStats> {
        let st = self.fs.state.lock();
        let sb = &self.fs.sb;
        Some(FsStats {
            block_size: BLOCK_SIZE as u64,
            total_blocks: sb.total_blocks,
            free_blocks: st.free_blocks,
            total_inodes: sb.inode_count,
            free_inodes: st.free_inodes,
        })
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.file_only()?;
        let mut st = self.fs.state.lock();
        self.fs.read_at(&mut st, self.ino, offset, buf)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        self.file_only()?;
        let mut st = self.fs.state.lock();
        self.fs.write_at(&mut st, self.ino, offset, buf)
    }

    fn truncate(&self) -> Result<()> {
        self.file_only()?;
        let mut st = self.fs.state.lock();
        let mut inode = self.fs.read_inode(self.ino)?;
        self.fs.free_data(&mut st, &mut inode)?;
        inode.mtime = tmpfs::now();
        self.fs.write_inode(self.ino, &inode)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        self.dir_only()?;
        let mut st = self.fs.state.lock();
        let entry = self.fs.find(&mut st, self.ino, name)?.ok_or(FsError::NotFound)?;
        Ok(self.fs.node(&mut st, entry.ino, kind_of(entry.kind)))
    }

    fn create(&self, name: &str, kind: Kind) -> Result<Arc<dyn Inode>> {
        self.dir_only()?;
        let code = match kind {
            Kind::File => KIND_FILE,
            Kind::Dir => KIND_DIR,
            _ => return Err(FsError::Invalid),
        };
        if name.len() > MAX_NAME {
            return Err(FsError::NameTooLong);
        }
        let fs = &self.fs;
        let mut st = fs.state.lock();
        if fs.find(&mut st, self.ino, name)?.is_some() {
            return Err(FsError::Exists);
        }
        let ino = fs.alloc_inode(&mut st, code)?;
        if let Err(e) = fs.add_entry(&mut st, self.ino, name, ino, code) {
            let _ = fs.free_inode(&mut st, ino);
            return Err(e);
        }
        Ok(fs.node(&mut st, ino, kind))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        self.dir_only()?;
        let fs = &self.fs;
        let mut st = fs.state.lock();
        let entry = fs.find(&mut st, self.ino, name)?.ok_or(FsError::NotFound)?;
        if entry.kind == KIND_DIR && !fs.entries(&mut st, entry.ino)?.is_empty() {
            return Err(FsError::NotEmpty);
        }
        fs.clear_entry(&mut st, self.ino, entry.slot)?;
        fs.drop_link(&mut st, entry.ino)
    }

    fn entry(&self, index: usize) -> Result<Option<(String, Kind)>> {
        self.dir_only()?;
        let mut st = self.fs.state.lock();
        Ok(self.fs.entries(&mut st, self.ino)?.into_iter().nth(index).map(|e| (e.name, kind_of(e.kind))))
    }

    fn is_empty(&self) -> bool {
        let mut st = self.fs.state.lock();
        self.fs.entries(&mut st, self.ino).map_or(true, |e| e.is_empty())
    }

    fn rename(&self, old: &str, new_dir: &Arc<dyn Inode>, new: &str) -> Result<()> {
        self.dir_only()?;
        let target = new_dir.as_any().downcast_ref::<Node>().ok_or(FsError::CrossDevice)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(FsError::CrossDevice);
        }
        target.dir_only()?;
        if new.len() > MAX_NAME {
            return Err(FsError::NameTooLong);
        }
        let fs = &self.fs;
        let mut st = fs.state.lock();
        let moving = fs.find(&mut st, self.ino, old)?.ok_or(FsError::NotFound)?;
        if let Some(existing) = fs.find(&mut st, target.ino, new)? {
            if existing.ino == moving.ino {
                return Ok(()); // the same file under both names
            }
            match (moving.kind, existing.kind) {
                (KIND_DIR, KIND_DIR) if !fs.entries(&mut st, existing.ino)?.is_empty() => return Err(FsError::NotEmpty),
                (KIND_DIR, KIND_DIR) => {}
                (KIND_DIR, _) => return Err(FsError::NotDir),
                (_, KIND_DIR) => return Err(FsError::IsDir),
                _ => {}
            }
            fs.clear_entry(&mut st, target.ino, existing.slot)?;
            fs.drop_link(&mut st, existing.ino)?;
        }
        if self.ino == target.ino {
            fs.write_entry(&mut st, self.ino, moving.slot, moving.ino, moving.kind, new)
        } else {
            // New name first: a crash in between leaves two names, not none.
            fs.add_entry(&mut st, target.ino, new, moving.ino, moving.kind)?;
            fs.clear_entry(&mut st, self.ino, moving.slot)
        }
    }
}
