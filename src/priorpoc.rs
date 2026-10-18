//! `fade-priorpoc-2r` (2026-10-18, MON-016) — pure decision; `main.rs` wires
//! the I/O. The 2026-10-17 tick sweep's variant A winner
//! (`bilore-ml-rs` `backtest_profile_sweep`, docs/findings/2026-10-17-profile-sweep.md):
//! fade-poc-wide's rule — fade toward the PRIOR RTH POC, `trade_setup`'s
//! entry, fill required, no re-take of a stopped level — with the prior
//! session's value area cut at 60% of its volume and a 40-tick stop / 2R
//! target. RTH only; like the sweep, the trade ends at 15:00 CT.

use bilore_core::market_structure::Direction;
use bilore_core::shadow_trader::{self, ShadowTrade};
use bilore_ml_rs::poc_anchor::{Geometry, TargetRule};
use bilore_ml_rs::trade_setup::{ProfileLevels, TradeSetupConfig};
use chrono::NaiveTime;
use rust_decimal::Decimal;

use crate::globex::GlobexSetup;

/// The prior session's value area covers this share of its volume.
pub const VA_PCT: u64 = 60;

/// 40-tick (10-point) stop, target 2x the stop distance.
pub fn geometry() -> Geometry {
    Geometry { stop_ticks: Some(Decimal::from(40)), target: TargetRule::R(Decimal::from(2)) }
}

/// The setup to arm at `price` against the prior RTH profile (60% value
/// area), or why there is none.
pub fn setup(
    price: Decimal,
    prior: &ProfileLevels,
    stopped: &[(Direction, Decimal)],
    cfg: &TradeSetupConfig,
) -> Result<GlobexSetup, String> {
    crate::wide::prior_poc_setup(geometry(), price, prior, stopped, cfg)
}

/// Entry trust stored as the row's confidence (registry: fade-poc's
/// formula with this strategy's own stop distance).
pub fn trust(price: Decimal, prior: &ProfileLevels, setup: &GlobexSetup) -> f64 {
    bilore_core::fade::fade_trust(price, prior.poc, (setup.entry - setup.stop).abs())
}

/// 08:30 <= t < 15:00 CT.
pub fn in_rth(t: NaiveTime) -> bool {
    let hm = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
    t >= hm(8, 30) && t < hm(15, 0)
}

/// End the trade at the RTH close the way the sweep does: an entered trade
/// is closed at `last_rth`, a pending one expires. `true` if it changed.
pub fn close_at_rth_end(trade: &mut ShadowTrade, last_rth: Decimal, tick_size: Decimal, tick_value: Decimal) -> bool {
    shadow_trader::close_at_mark(trade, last_rth, tick_size, tick_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilore_core::shadow_trader::Outcome;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn t(h: u32, m: u32, s: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, s).unwrap()
    }

    // 09-29 RTH profile: POC 7732.50, VAH 7745.00, VAL 7722.75.
    fn prior() -> ProfileLevels {
        ProfileLevels { poc: d("7732.50"), vah: d("7745.00"), val: d("7722.75"), ib_high: None, ib_low: None }
    }

    #[test]
    fn above_the_prior_poc_it_sells_with_a_40_tick_stop_and_2r_target() {
        let s = setup(d("7740.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!((s.direction, s.anchor.as_str()), (Direction::Short, "VAH"));
        assert_eq!(s.entry, d("7746.50"), "trade_setup's entry: VAH + 6 ticks");
        assert_eq!(s.stop, d("7756.50"), "40 ticks above");
        assert_eq!(s.target, d("7726.50"), "80 ticks below");
    }

    #[test]
    fn below_the_prior_poc_it_buys_with_a_40_tick_stop() {
        let s = setup(d("7728.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!(s.direction, Direction::Long);
        assert_eq!(s.entry, d("7721.25"), "VAL - 6 ticks");
        assert_eq!((s.stop, s.target), (d("7711.25"), d("7741.25")));
    }

    #[test]
    fn a_stopped_level_is_not_retaken_by_priorpoc() {
        let stopped = [(Direction::Short, d("7746.50"))];
        let err = setup(d("7740.00"), &prior(), &stopped, &TradeSetupConfig::default()).unwrap_err();
        assert!(err.contains("7746.5"), "{err}");
    }

    #[test]
    fn it_trades_only_from_08_30_to_15_00() {
        let got: Vec<bool> = [t(8, 29, 59), t(8, 30, 0), t(14, 59, 59), t(15, 0, 0), t(20, 0, 0)].map(in_rth).to_vec();
        assert_eq!(got, [false, true, true, false, false]);
    }

    fn short_at_7746_50() -> ShadowTrade {
        ShadowTrade::new(Direction::Short, d("7746.50"), d("7746.50"), d("7756.50"), Some(d("7726.50")), 1, true)
    }

    #[test]
    fn at_15_00_an_entered_trade_closes_at_the_last_rth_price() {
        let mut trade = short_at_7746_50();
        trade.outcome = Outcome::Entered;
        trade.entry_price = Some(d("7746.50"));
        assert!(close_at_rth_end(&mut trade, d("7740.00"), d("0.25"), d("1.25")));
        assert_eq!(trade.outcome, Outcome::Expired);
        assert_eq!(trade.exit_price, Some(d("7740.00")));
        assert_eq!(trade.pnl_ticks, Some(d("26")));
    }

    #[test]
    fn at_15_00_a_pending_setup_expires() {
        let mut trade = short_at_7746_50();
        assert!(close_at_rth_end(&mut trade, d("7740.00"), d("0.25"), d("1.25")));
        assert_eq!((trade.outcome, trade.exit_price), (Outcome::Expired, None));
    }

    #[test]
    fn priorpoc_trust_is_the_extension_beyond_the_prior_poc_over_its_40_tick_stop() {
        let s = setup(d("7740.00"), &prior(), &[], &TradeSetupConfig::default()).unwrap();
        assert_eq!(trust(d("7740.00"), &prior(), &s), 0.875);
    }
}
