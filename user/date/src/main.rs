//! date: the current date and time (UTC).

#![no_std]
#![no_main]

use ulib::println;
use ulib::time::DateTime;

#[no_mangle]
fn main() -> i32 {
    match ulib::time_ms() {
        0 => {
            println!("date: the clock is not available");
            1
        }
        ms => {
            println!("{} UTC", DateTime::from_unix(ms / 1000));
            0
        }
    }
}
