//! Prior-session levels built from recorded ticks (2026-10-02) — pure.
//!
//! `session_profiles`' live `MESZ6` row isn't an RTH profile: tpo-builder's
//! RTH session has a start (13:30 UTC) but no end, so every trade until the
//! UTC date rolls (19:00 CT) — the post-close hour and early Globex — is
//! added to that day's "RTH" profile. On 10-01 the true RTH profile is POC
//! 7690 / VAH 7723.25 / VAL 7677.25; by evening the row read 7730 / 7739 /
//! 7693.50, and the RTH strategies load it at midnight. Ticks are recorded
//! all session (tick_recorder watchdog, 09-30), so the monitor builds the
//! 08:30-15:00 profile itself, the same way the tick backtests do, and only
//! falls back to `session_profiles` when the day isn't fully recorded.

use bilore_ml_rs::poc_anchor::VolumeProfile;
use bilore_ml_rs::trade_setup::ProfileLevels;
use chrono::NaiveTime;
use rust_decimal::Decimal;

/// RTH POC/VAH/VAL from that session's volume at price, or `None` when the
/// recording doesn't span the session (first tick after 08:35 CT or last
/// before 14:55 CT) or is empty.
pub fn rth_levels_from_ticks(volume_at_price: &[(Decimal, i64)], first: NaiveTime, last: NaiveTime) -> Option<ProfileLevels> {
    let hm = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
    if first > hm(8, 35) || last < hm(14, 55) {
        return None;
    }
    let mut vp = VolumeProfile::default();
    for (price, volume) in volume_at_price {
        vp.add(*price, (*volume).max(0) as u64);
    }
    let (val, vah) = vp.value_area()?;
    Some(ProfileLevels { poc: vp.poc()?, vah, val, ib_high: None, ib_low: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn rows() -> Vec<(Decimal, i64)> {
        vec![(d("99.75"), 10), (d("100.00"), 30), (d("100.25"), 40), (d("100.50"), 20)]
    }

    #[test]
    fn a_fully_recorded_session_gives_its_poc_and_value_area() {
        let l = rth_levels_from_ticks(&rows(), t(8, 30), t(14, 59)).expect("levels");
        assert_eq!((l.poc, l.val, l.vah), (d("100.25"), d("100.00"), d("100.25")));
        assert_eq!((l.ib_high, l.ib_low), (None, None));
    }

    #[test]
    fn a_partly_recorded_session_is_not_trusted() {
        assert!(rth_levels_from_ticks(&rows(), t(9, 10), t(14, 59)).is_none(), "late start");
        assert!(rth_levels_from_ticks(&rows(), t(8, 30), t(13, 0)).is_none(), "early stop");
    }

    #[test]
    fn no_ticks_no_levels() {
        assert!(rth_levels_from_ticks(&[], t(8, 30), t(14, 59)).is_none());
    }
}
