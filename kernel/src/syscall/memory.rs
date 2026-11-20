//! Memory system calls (see mm/uspace.rs for the model).

use super::{Errno, SysResult, EFAULT, EINVAL};
use crate::mm::uspace::{Prot, UserSpace};
use crate::task;

const MAP_PRIVATE: u64 = 0x02;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

/// Run `f` on the calling process's memory, then flush whatever mappings it
/// changed from the CPUs running the process's other threads.
fn change_space<R>(f: impl FnOnce(&mut UserSpace) -> R) -> Result<R, Errno> {
    let process = task::current_process().ok_or(EFAULT)?;
    let (result, flush) = {
        let mut space = process.space.lock();
        let result = f(&mut space);
        (result, space.take_flush())
    };
    drop(process);
    if let Some(flush) = flush {
        flush.finish();
    }
    Ok(result)
}

pub fn brk(addr: u64) -> SysResult {
    change_space(|s| s.brk(addr))
}

/// Anonymous private memory only: `flags` must be MAP_PRIVATE |
/// MAP_ANONYMOUS, optionally with MAP_FIXED.
pub fn mmap(addr: u64, len: u64, prot: u64, flags: u64) -> SysResult {
    let prot = Prot::from_bits(prot).ok_or(EINVAL)?;
    let required = MAP_PRIVATE | MAP_ANONYMOUS;
    if flags & !(required | MAP_FIXED) != 0 || flags & required != required {
        return Err(EINVAL);
    }
    let fixed = (flags & MAP_FIXED != 0).then_some(addr);
    Ok(change_space(|s| s.mmap(fixed, len, prot))??)
}

pub fn munmap(addr: u64, len: u64) -> SysResult {
    change_space(|s| s.munmap(addr, len))??;
    Ok(0)
}

pub fn mprotect(addr: u64, len: u64, prot: u64) -> SysResult {
    let prot = Prot::from_bits(prot).ok_or(EINVAL)?;
    change_space(|s| s.mprotect(addr, len, prot))??;
    Ok(0)
}
