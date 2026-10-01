//! `fade-poc-globex` (2026-09-30) — pure decisions; `main.rs` wires the I/O.
//!
//! Fade toward the just-finished RTH session's POC during Globex
//! (17:00 -> 08:30 CT): Short above it, Long at/below. Entry from the live
//! `trade_setup` over that session's POC/VAH/VAL; stop and target from the
//! backtest's `poc_anchor::apply_geometry` (60-tick stop, 2R) — so the
//! live rule is the backtested rule (`backtest_poc_geometry`, Globex GA).
//! No model gate. A level stopped out this Globex session is not re-taken
//! in the same direction.

use bilore_core::fade::fade_direction;
use bilore_core::market_structure::Direction;
use bilore_ml_rs::clean_sessions::session_of;
use bilore_ml_rs::poc_anchor::{apply_geometry, ungated_risk, Geometry, TargetRule};
use bilore_ml_rs::trade_setup::{trade_setup, ProfileLevels, SetupResult, TradeSetupConfig};
use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;

/// 60-tick (15-point) stop, target 2x the stop distance.
pub fn geometry() -> Geometry {
    Geometry { stop_ticks: Some(Decimal::from(60)), target: TargetRule::R(Decimal::from(2)) }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GlobexSetup {
    pub direction: Direction,
    pub anchor: String,
    pub entry: Decimal,
    pub stop: Decimal,
    pub target: Decimal,
}

/// The trading date of the Globex session `now_ct` falls in (17:00 belongs
/// to the next trading day, Friday evening to Monday), `None` in RTH and in
/// the 15:00-17:00 gap.
pub fn globex_session(now_ct: NaiveDateTime) -> Option<NaiveDate> {
    match session_of(now_ct) {
        Some((date, false)) => Some(date),
        _ => None,
    }
}

/// The setup to arm at `price`, or why there is none.
pub fn globex_setup(
    price: Decimal,
    prior: &ProfileLevels,
    stopped: &[(Direction, Decimal)],
    cfg: &TradeSetupConfig,
) -> Result<GlobexSetup, String> {
    let direction = fade_direction(price, prior.poc);
    let s = match trade_setup(direction, price, prior, &ungated_risk(direction), None, cfg) {
        SetupResult::Setup(s) => s,
        SetupResult::NoSetup(reason) => return Err(reason),
    };
    if stopped.contains(&(direction, s.entry_price)) {
        return Err(format!("{direction:?} at {} was stopped out earlier this session", s.entry_price));
    }
    let (stop, target) =
        apply_geometry(geometry(), direction, s.entry_price, s.stop, s.targets.first().map(|t| t.price), cfg.tick_size);
    Ok(GlobexSetup {
        direction,
        anchor: s.anchor_lbl,
        entry: s.entry_price,
        stop,
        target: target.ok_or("no target")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn at(date: &str, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    // 09-29 RTH profile: POC 7732.50, VAH 7745.00, VAL 7722.75.
    fn prior() -> ProfileLevels {
        ProfileLevels { poc: d("7732.50"), vah: d("7745.00"), val: d("7722.75"), ib_high: None, ib_low: None }
    }

    #[test]
    fn globex_hours_map_to_the_next_trading_day() {
        assert_eq!(globex_session(at("2026-09-29", 20, 0)), Some(day("2026-09-30")));
        assert_eq!(globex_session(at("2026-09-30", 3, 0)), Some(day("2026-09-30")));
        assert_eq!(globex_session(at("2026-10-02", 18, 0)), Some(day("2026-10-05")), "Friday evening -> Monday");
        assert_eq!(globex_session(at("2026-09-30", 10, 0)), None, "RTH");
        assert_eq!(globex_session(at("2026-09-30", 16, 30)), None, "halt");
    }

    #[test]
    fn above_the_poc_it_sells_at_the_vah_with_a_60_tick_stop_and_2r_target() {
        let s = globex_setup(d("7740.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!((s.direction, s.anchor.as_str()), (Direction::Short, "VAH"));
        assert_eq!(s.entry, d("7746.50"), "VAH + 6 ticks");
        assert_eq!(s.stop, d("7761.50"), "15 points above");
        assert_eq!(s.target, d("7716.50"), "30 points below");
    }

    #[test]
    fn below_the_poc_it_buys_at_the_val() {
        let s = globex_setup(d("7728.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!((s.direction, s.anchor.as_str()), (Direction::Long, "VAL"));
        assert_eq!(s.entry, d("7721.25"), "VAL - 6 ticks");
        assert_eq!((s.stop, s.target), (d("7706.25"), d("7751.25")));
    }

    #[test]
    fn a_level_stopped_out_this_session_is_not_retaken_in_that_direction() {
        let stopped = vec![(Direction::Short, d("7746.50"))];
        assert!(globex_setup(d("7740.00"), &prior(), &stopped, &TradeSetupConfig::default()).is_err());
        let other_side = vec![(Direction::Long, d("7746.50"))];
        assert!(globex_setup(d("7740.00"), &prior(), &other_side, &TradeSetupConfig::default()).is_ok());
    }

    #[test]
    fn far_above_every_level_there_is_no_setup() {
        let err = globex_setup(d("7800.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap_err();
        assert!(err.contains("no level at/above"), "{err}");
    }
}
