//! df [path...]: size and free space of filesystems (by default / and every
//! disk mounted under it as /disk, /disk1, ...).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String, vec::Vec};
use ulib::fs::{self, Kind};
use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let mut paths: Vec<String> = ulib::args().skip(1).map(String::from).collect();
    if paths.is_empty() {
        paths.push(String::from("/"));
        for entry in fs::read_dir("/").into_iter().flatten().flatten() {
            if entry.kind == Kind::Dir && entry.name().starts_with("disk") {
                paths.push(format!("/{}", entry.name()));
            }
        }
    }
    println!("{:<12} {:>10} {:>10} {:>10} {:>5} {:>11}", "FILESYSTEM", "SIZE KiB", "USED KiB", "FREE KiB", "USE%", "FREE FILES");
    let mut failed = false;
    for path in &paths {
        match fs::statfs(path) {
            Ok(s) => {
                let kib = |blocks: u64| blocks * s.block_size / 1024;
                let used = s.total_blocks - s.free_blocks;
                let percent = (used * 100).checked_div(s.total_blocks).unwrap_or(0);
                // The in-memory root has no limit on the number of files.
                let files = if s.total_inodes == 0 { String::from("-") } else { format!("{}", s.free_inodes) };
                println!("{:<12} {:>10} {:>10} {:>10} {:>4}% {:>11}", path, kib(s.total_blocks), kib(used), kib(s.free_blocks), percent, files);
            }
            Err(e) => {
                println!("df: {}: {}", path, e);
                failed = true;
            }
        }
    }
    failed as i32
}
