//! `fade-poc-wide` (2026-10-12, MON-014) — pure decision; `main.rs` wires the
//! I/O. Replaces `fade-poc-fill`: the same entries (fade toward the PRIOR RTH
//! POC, `trade_setup`'s entry anchor, fill required, RTH, no model gate) with
//! the stop/target sweep's best geometry for that rule — an 80-tick stop and
//! a 2R target (`backtest_poc_geometry`: 286 trades over a year, 31.9%, +2.1
//! ticks/trade, +$736; H1 +$1,510, H2 -$774). A level stopped out today is
//! not re-taken in the same direction.

use bilore_core::fade::fade_direction;
use bilore_core::market_structure::Direction;
use bilore_ml_rs::poc_anchor::{apply_geometry, ungated_risk, Geometry, TargetRule};
use bilore_ml_rs::trade_setup::{trade_setup, ProfileLevels, SetupResult, TradeSetupConfig};
use rust_decimal::Decimal;

use crate::globex::GlobexSetup;

/// 80-tick (20-point) stop, target 2x the stop distance.
pub fn geometry() -> Geometry {
    Geometry { stop_ticks: Some(Decimal::from(80)), target: TargetRule::R(Decimal::from(2)) }
}

/// The setup to arm at `price` against the prior RTH profile, or why there is none.
pub fn wide_setup(
    price: Decimal,
    prior: &ProfileLevels,
    stopped: &[(Direction, Decimal)],
    cfg: &TradeSetupConfig,
) -> Result<GlobexSetup, String> {
    prior_poc_setup(geometry(), price, prior, stopped, cfg)
}

/// Today's prior-POC fade entry with `geometry`'s stop and target — shared
/// by fade-poc-wide and fade-priorpoc-2r (`priorpoc`).
pub fn prior_poc_setup(
    geometry: Geometry,
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
        return Err(format!("{direction:?} at {} was stopped out earlier today", s.entry_price));
    }
    let (stop, target) =
        apply_geometry(geometry, direction, s.entry_price, s.stop, s.targets.first().map(|t| t.price), cfg.tick_size);
    Ok(GlobexSetup { direction, anchor: s.anchor_lbl, entry: s.entry_price, stop, target: target.ok_or("no target")? })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    // 09-29 RTH profile: POC 7732.50, VAH 7745.00, VAL 7722.75.
    fn prior() -> ProfileLevels {
        ProfileLevels { poc: d("7732.50"), vah: d("7745.00"), val: d("7722.75"), ib_high: None, ib_low: None }
    }

    #[test]
    fn above_the_prior_poc_it_sells_with_an_80_tick_stop_and_2r_target() {
        let s = wide_setup(d("7740.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!((s.direction, s.anchor.as_str()), (Direction::Short, "VAH"));
        assert_eq!(s.entry, d("7746.50"), "trade_setup's entry: VAH + 6 ticks");
        assert_eq!(s.stop, d("7766.50"), "80 ticks above");
        assert_eq!(s.target, d("7706.50"), "160 ticks below");
    }

    #[test]
    fn below_the_prior_poc_it_buys_with_an_80_tick_stop() {
        let s = wide_setup(d("7728.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!(s.direction, Direction::Long);
        assert_eq!(s.entry, d("7721.25"), "VAL - 6 ticks");
        assert_eq!((s.stop, s.target), (d("7701.25"), d("7761.25")));
    }

    #[test]
    fn a_level_stopped_out_today_is_not_retaken() {
        let stopped = [(Direction::Short, d("7746.50"))];
        let err = wide_setup(d("7740.00"), &prior(), &stopped, &TradeSetupConfig::default()).unwrap_err();
        assert!(err.contains("7746.5"), "{err}");
    }
}
