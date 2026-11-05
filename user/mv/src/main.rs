//! mv <from> <to>: rename or move a file or directory. If <to> is an
//! existing directory, <from> moves into it.

#![no_std]
#![no_main]

extern crate alloc;

use ulib::fs::{self, Kind};
use ulib::println;

#[no_mangle]
fn main() -> i32 {
    let (Some(from), Some(to), None) = (ulib::args().nth(1), ulib::args().nth(2), ulib::args().nth(3)) else {
        println!("usage: mv <from> <to>");
        return 2;
    };
    let target = match fs::stat(to) {
        Ok(s) if s.kind == Kind::Dir => {
            let name = from.trim_end_matches('/').rsplit('/').next().unwrap_or(from);
            alloc::format!("{}/{}", to.trim_end_matches('/'), name)
        }
        _ => alloc::string::String::from(to),
    };
    match fs::rename(from, &target) {
        Ok(()) => 0,
        Err(e) => {
            println!("mv: {} -> {}: {}", from, target, e);
            1
        }
    }
}
