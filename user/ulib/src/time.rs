//! Calendar dates from Unix time (UTC).

use core::fmt;

/// A moment as year, month, day, hour, minute, second (UTC).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl DateTime {
    pub fn from_unix(seconds: u64) -> DateTime {
        let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
        let rest = (seconds % 86_400) as u32;
        DateTime { year, month, day, hour: rest / 3600, minute: rest / 60 % 60, second: rest % 60 }
    }
}

/// `2026-11-05 14:03:59`
impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }
}

/// Days since 1970-01-01 -> (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (month <= 2) as i64, month, day)
}
