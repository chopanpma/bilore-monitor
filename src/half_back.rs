//! `half-back-rth` (MON-028, 2026-11-07) — pure decisions; `main.rs` wires the I/O.
//!
//! The Market Profile half-back (bilore-ml-rs ML-011): the reference is the
//! high-low range of the last 4 completed 30-min periods (`RollingLevels`,
//! the fade-roll periods), 64+ ticks. One tick past its midpoint arms a
//! stop-type entry toward the far extreme; target one tick before the far
//! extreme, stop one tick past the near one. RTH only. The rule itself
//! (`arm_direction`, `plan`, `fill_zone`, `jumped_past`) is
//! `bilore_ml_rs::half_back`, the code the backtest runs.

use crate::rolling::{self, RollingLevels, Window};
use bilore_core::market_structure::Direction;
use bilore_core::shadow_trader::{Outcome, ShadowTrade};
use bilore_ml_rs::half_back::{arm_direction, fill_zone, jumped_past, plan, Params, Plan, RefRange, Stop};
use chrono::{Duration, NaiveDateTime, NaiveTime, Timelike};
use rust_decimal::Decimal;

/// Completed 30-min periods in the reference.
pub const PERIODS: usize = 4;

/// The evidence's geometry (findings 2026-11-06-half-back, RTH / last-4 / full / min 64).
pub fn params() -> Params {
    Params { stop: Stop::Full, min_range_ticks: Decimal::from(64), max_risk_ticks: Decimal::from(80), tick: Decimal::new(25, 2) }
}

/// RTH 08:30-15:00 CT.
pub fn in_window(t: NaiveTime) -> bool {
    rolling::window_of(t) == Some(Window::Rth)
}

/// A reference key: the period start in minutes (CT wall clock).
fn key_of(start: NaiveDateTime) -> i64 {
    start.and_utc().timestamp() / 60
}

/// The reference now: the last 4 completed periods, keyed by the latest one's start.
pub fn reference(r: &RollingLevels) -> Option<RefRange> {
    let (start, high, low) = r.range(PERIODS)?;
    Some(RefRange { key: key_of(start), high, low })
}

/// The pending trade to arm at `price`, or why there is none.
pub fn setup(price: Decimal, r: &RefRange, used: &[(i64, Direction)]) -> Result<(ShadowTrade, Plan), String> {
    let p = params();
    let direction = arm_direction(price, r).ok_or("price at the half-back")?;
    if used.contains(&(r.key, direction)) {
        return Err(format!("{direction:?} already taken on this reference"));
    }
    let plan = plan(r, direction, &p).ok_or_else(|| format!("range {}-{} under 64 ticks or stop over 80", r.low, r.high))?;
    let (lo, hi) = fill_zone(&plan, p.tick);
    Ok((ShadowTrade::new(direction, lo, hi, plan.stop, Some(plan.target), 1, true), plan))
}

/// Whether a pending trade armed on reference `trade_key` must be cancelled:
/// the reference changed, or price jumped past its fill zone.
pub fn cancels(trade: &ShadowTrade, trade_key: Option<i64>, reference_key: Option<i64>, price: Decimal) -> bool {
    trade.outcome == Outcome::Pending && (trade_key != reference_key || jumped_past(trade, price))
}

/// Mark a pending trade invalidated; `false` if it was not pending.
pub fn cancel(trade: &mut ShadowTrade) -> bool {
    if trade.outcome != Outcome::Pending {
        return false;
    }
    trade.outcome = Outcome::Invalidated;
    true
}

/// The reference key in force when a setup was armed at `ct`: the period
/// completed before the one in progress.
pub fn key_at(ct: NaiveDateTime) -> i64 {
    let start = ct.with_minute(if ct.minute() < 30 { 0 } else { 30 }).and_then(|t| t.with_second(0)).unwrap_or(ct);
    let start = start.with_nanosecond(0).unwrap_or(start);
    key_of(start - Duration::minutes(30))
}

/// Restart restore: the (reference, direction) pairs today's rows used.
pub fn used_from_signals(signals: &[(Direction, NaiveDateTime)]) -> Vec<(i64, Direction)> {
    signals.iter().map(|&(direction, ct)| (key_at(ct), direction)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn at(h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 9).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn reference_80() -> RefRange {
        RefRange { key: key_of(at(10, 0)), high: d("7720"), low: d("7700") }
    }

    #[test]
    fn the_reference_is_the_last_four_periods() {
        let mut r = RollingLevels::default();
        for (h, m, p) in [(8, 35, "7705"), (9, 5, "7720"), (9, 35, "7700"), (10, 5, "7712")] {
            r.on_tick(at(h, m), d(p), 1);
        }
        r.on_tick(at(10, 35), d("7710"), 1); // completes 10:00
        assert_eq!(reference(&r), Some(RefRange { key: key_of(at(10, 0)), high: d("7720"), low: d("7700") }));
        let mut three = RollingLevels::default();
        for (h, m, p) in [(9, 5, "7720"), (9, 35, "7700"), (10, 5, "7712"), (10, 35, "7710")] {
            three.on_tick(at(h, m), d(p), 1);
        }
        assert_eq!(reference(&three), None);
    }

    #[test]
    fn a_setup_is_a_stop_entry_toward_the_far_extreme() {
        let (t, _) = setup(d("7715"), &reference_80(), &[]).unwrap();
        assert_eq!((t.direction, t.outcome), (Direction::Short, Outcome::Pending));
        assert_eq!((t.entry_lo, t.entry_hi, t.stop, t.target_1), (d("7709.25"), d("7709.75"), d("7720.25"), Some(d("7700.25"))));
        let (t, _) = setup(d("7705"), &reference_80(), &[]).unwrap();
        assert_eq!((t.direction, t.entry_lo, t.entry_hi, t.stop, t.target_1), (Direction::Long, d("7710.25"), d("7710.75"), d("7699.75"), Some(d("7719.75"))));
    }

    #[test]
    fn a_narrow_reference_or_a_used_direction_arms_nothing() {
        let narrow = RefRange { key: 1, high: d("7715"), low: d("7700") }; // 60 ticks
        assert!(setup(d("7712"), &narrow, &[]).is_err());
        let r = reference_80();
        assert!(setup(d("7715"), &r, &[(r.key, Direction::Short)]).is_err());
        assert!(setup(d("7705"), &r, &[(r.key, Direction::Short)]).is_ok(), "the other direction stays open");
    }

    #[test]
    fn only_rth_opens_setups() {
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert_eq!([in_window(t(8, 29)), in_window(t(8, 30)), in_window(t(14, 59)), in_window(t(15, 0))], [false, true, true, false]);
    }

    #[test]
    fn the_config_pins_the_evidence() {
        assert_eq!(PERIODS, 4);
        assert_eq!(params(), Params { stop: Stop::Full, min_range_ticks: d("64"), max_risk_ticks: d("80"), tick: d("0.25") });
    }

    #[test]
    fn a_new_reference_or_a_jump_cancels_a_pending_setup() {
        let a = reference_80();
        let (mut t, _) = setup(d("7715"), &a, &[]).unwrap();
        assert!(!cancels(&t, Some(a.key), Some(a.key), d("7712")));
        assert!(cancels(&t, Some(a.key), Some(a.key + 30), d("7712")), "a new reference");
        assert!(cancels(&t, Some(a.key), None, d("7712")), "no reference");
        assert!(cancels(&t, Some(a.key), Some(a.key), d("7709.00")), "a jump past the zone");
        let mut entered = t;
        entered.outcome = Outcome::Entered;
        assert!(!cancels(&entered, Some(a.key), Some(a.key + 30), d("7709.00")));
        assert!(cancel(&mut t));
        assert_eq!(t.outcome, Outcome::Invalidated);
        assert!(!cancel(&mut entered));
    }

    #[test]
    fn a_restart_restores_the_used_directions() {
        let used = used_from_signals(&[(Direction::Short, at(9, 47)), (Direction::Long, at(10, 2))]);
        assert_eq!(used, vec![(key_of(at(9, 0)), Direction::Short), (key_of(at(9, 30)), Direction::Long)]);
    }
}
