//! PTU (Programme Time Unit) helpers. The Dutch imbalance market settles per 15 minutes.

use chrono::{DateTime, Duration, Timelike, Utc};

pub const PTU_MINUTES: i64 = 15;
pub const PTU_HOURS: f64 = 0.25;
pub const PTUS_PER_DAY: usize = 96;

pub fn ptu() -> Duration {
    Duration::minutes(PTU_MINUTES)
}

/// Fractional hour of day, e.g. 13.25 for 13:15. (We treat UTC as local time to keep it simple.)
pub fn hour_of_day(ts: DateTime<Utc>) -> f64 {
    ts.hour() as f64 + ts.minute() as f64 / 60.0
}

/// Index of the PTU within its day: 0..96.
pub fn ptu_of_day(ts: DateTime<Utc>) -> usize {
    (ts.hour() as usize * 60 + ts.minute() as usize) / PTU_MINUTES as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn counts_quarter_hours_from_midnight() {
        let ts = Utc.with_ymd_and_hms(2026, 11, 5, 13, 29, 59).unwrap();
        assert_eq!(ptu_of_day(ts), 53);
    }
}
