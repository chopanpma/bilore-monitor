//! The fade-toward-POC shadow strategy's decision rule — pure, tested.
//!
//! Origin: model-analysis.md §6.10 — with real tick-verified fills, the
//! model's direction showed no skill (20.0% vs random's 17.4%), but
//! fade-toward-POC was the only profitable variant tested (58.3% win rate,
//! +146 ticks; small sample — a lead, not an edge) while its exact mirror,
//! momentum-away, went 0/23. This rule mirrors the validated
//! `bilore-ml-rs/src/bin/backtest_tick_sim_direction.rs` fade arm exactly:
//!
//! ```rust,ignore
//! if row.open > row.prior_poc { Short } else { Long }
//! ```
//!
//! Everything around the direction pick (risk sizing from the trained
//! model's `risk_params`, `trade_setup` geometry, shadow tracking) stays
//! identical to the `ml-model` arm — only direction selection differs, same
//! as in the backtest. Live deviations from the backtest (one trade per
//! session instead of per-period evaluation) are documented in
//! `bilore-project-conf/contracts/strategies.md`.

use bilore_core::market_structure::Direction;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Fade direction: Short when `price` is above the prior session's POC
/// (expecting a rotate back down into value), Long at or below it.
/// Strict `>` — price exactly at POC fades Long, matching the backtest's
/// `row.open > row.prior_poc` comparison.
pub fn fade_direction(price: Decimal, prior_poc: Decimal) -> Direction {
    if price > prior_poc {
        Direction::Short
    } else {
        Direction::Long
    }
}

/// Entry trust for a fade signal, 0.5–1.0: how far price has extended
/// beyond the prior POC relative to the setup's stop distance, mapped
/// onto `[0.5, 1.0]`. 50% = price at the POC — the rule's coin-flip
/// boundary (no extension, no information); 100% = price extended a full
/// stop beyond it. A normalized extension score, NOT a calibrated win
/// probability (see `bilore-project-conf/contracts/strategies.md`,
/// "Trust metric"). Degenerate geometry (`stop_distance <= 0`) returns
/// the neutral 0.5 rather than dividing by zero.
pub fn fade_trust(price: Decimal, prior_poc: Decimal, stop_distance: Decimal) -> f64 {
    if stop_distance <= Decimal::ZERO {
        return 0.5;
    }
    let extension = ((price - prior_poc).abs() / stop_distance).clamp(Decimal::ZERO, Decimal::ONE);
    0.5 + 0.5 * extension.to_f64().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    #[test]
    fn price_above_poc_fades_short() {
        assert_eq!(fade_direction(d("5901.00"), d("5900.00")), Direction::Short);
    }

    #[test]
    fn price_at_poc_fades_long() {
        // Strict `>` in the source rule: equality goes Long.
        assert_eq!(fade_direction(d("5900.00"), d("5900.00")), Direction::Long);
    }

    #[test]
    fn price_below_poc_fades_long() {
        assert_eq!(fade_direction(d("5899.75"), d("5900.00")), Direction::Long);
    }

    #[test]
    fn far_extremes_follow_the_same_rule() {
        assert_eq!(fade_direction(d("6000.00"), d("5900.00")), Direction::Short);
        assert_eq!(fade_direction(d("5800.00"), d("5900.00")), Direction::Long);
    }

    #[test]
    fn trust_at_poc_is_the_coin_flip_boundary() {
        assert_eq!(fade_trust(d("5900.00"), d("5900.00"), d("4.00")), 0.5);
    }

    #[test]
    fn trust_scales_with_extension_beyond_poc() {
        // Half a stop beyond the POC -> 75%; a full stop -> 100%.
        assert!((fade_trust(d("5902.00"), d("5900.00"), d("4.00")) - 0.75).abs() < 1e-9);
        assert!((fade_trust(d("5904.00"), d("5900.00"), d("4.00")) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn trust_is_symmetric_above_and_below_poc() {
        // The fade rule fires on both sides of the POC — extension score
        // uses absolute distance, so below matches above.
        assert!((fade_trust(d("5898.00"), d("5900.00"), d("4.00")) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn trust_clamps_beyond_one_stop() {
        assert!((fade_trust(d("5910.00"), d("5900.00"), d("4.00")) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn trust_with_degenerate_stop_distance_is_neutral() {
        assert_eq!(fade_trust(d("5902.00"), d("5900.00"), Decimal::ZERO), 0.5);
    }
}
