//! ls [-l] [path...]: list directories (the working directory by default).
//! Directories are shown with a trailing '/', devices with '*', pipes '|'.
//! With -l, each line also shows the size and the modification time.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::{format, string::String};
use ulib::fs::{self, Kind, Stat};
use ulib::println;
use ulib::time::DateTime;

#[no_mangle]
fn main() -> i32 {
    let long = ulib::args().skip(1).any(|a| a == "-l");
    let paths = || ulib::args().skip(1).filter(|a| *a != "-l");
    let many = paths().count() > 1;
    let mut failed = false;
    let mut show = |path: &str| {
        if let Err(e) = list(path, long, many) {
            println!("ls: {}: {}", path, e);
            failed = true;
        }
    };
    if paths().next().is_none() {
        show(".");
    } else {
        paths().for_each(show);
    }
    failed as i32
}

fn suffix(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir => "/",
        Kind::Device => "*",
        Kind::Pipe => "|",
        Kind::File => "",
    }
}

fn line(name: &str, stat: &Stat, long: bool) -> String {
    if !long {
        return format!("{}{}", name, suffix(stat.kind));
    }
    let when = if stat.mtime == 0 { String::from("-") } else { format!("{}", DateTime::from_unix(stat.mtime)) };
    format!("{:>10}  {:<19}  {}{}", stat.size, when, name, suffix(stat.kind))
}

fn list(path: &str, long: bool, heading: bool) -> Result<(), ulib::Errno> {
    let stat = fs::stat(path)?;
    if stat.kind != Kind::Dir {
        println!("{}", line(path, &stat, long));
        return Ok(());
    }
    if heading {
        println!("{}:", path);
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let full = format!("{}/{}", path.trim_end_matches('/'), entry.name());
        // The entry may vanish between listing and stat: show what we know.
        let stat = fs::stat(&full).unwrap_or(Stat { kind: entry.kind, size: 0, mtime: 0 });
        println!("{}", line(entry.name(), &stat, long));
    }
    Ok(())
}
