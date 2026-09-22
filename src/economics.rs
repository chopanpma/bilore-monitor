//! Per-symbol tick economics (tick size, tick value in USD) — real CME
//! micro-futures contract specs, not the MES-only figures `main.rs` used
//! to hardcode for every registered symbol (found live: MNQZ6's shadow
//! trades were computing PnL/max-loss-dollars at MES's $1.25/tick instead
//! of MNQ's real $0.50/tick, a 2.5x overstatement).
//!
//! Matched by explicit contract-root prefix (`MESZ6`/`MESU6` -> `MES`),
//! not by stripping trailing month/year characters — CME month codes
//! (F/G/H/J/K/M/N/Q/U/V/X/Z) overlap with real root letters (`MNQ`'s own
//! `Q` is also August's month code), so a generic "strip trailing
//! month+year chars" trim over-strips `MNQZ6` down to `MN`. Explicit
//! prefix matching has no such ambiguity.

use rust_decimal::Decimal;

/// `(tick_size, tick_value_usd)` for one micro-futures contract, matched
/// by root prefix so a quarterly roll (`MESU6` -> `MESZ6`) never needs a
/// code change here. Unrecognized roots fall back to MES's economics with
/// a warning log — this only skews *displayed* PnL/risk figures for a
/// shadow (paper) trade, never places a real order, so a wrong-but-visible
/// number is an acceptable degradation versus refusing to track a new
/// symbol at all.
pub fn tick_economics(symbol: &str) -> (Decimal, Decimal) {
    if symbol.starts_with("MES") {
        ("0.25".parse().unwrap(), "1.25".parse().unwrap())
    } else if symbol.starts_with("MNQ") {
        ("0.25".parse().unwrap(), "0.50".parse().unwrap())
    } else if symbol.starts_with("MGC") {
        ("0.10".parse().unwrap(), "1.00".parse().unwrap())
    } else {
        tracing::warn!("{symbol}: no tick economics registered — falling back to MES's ($0.25/$1.25)");
        ("0.25".parse().unwrap(), "1.25".parse().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn mes_contract_has_1_25_tick_value() {
        assert_eq!(tick_economics("MESZ6"), (d("0.25"), d("1.25")));
        assert_eq!(tick_economics("MESU6"), (d("0.25"), d("1.25")));
    }

    #[test]
    fn mnq_contract_has_the_distinct_0_50_tick_value() {
        // The exact case a naive "strip trailing month/year letters" trim
        // gets wrong: MNQ's own root ends in 'Q', which is also a real CME
        // month code (August) — this must resolve via prefix match, not
        // trimming, or it silently truncates to "MN".
        assert_eq!(tick_economics("MNQZ6"), (d("0.25"), d("0.50")));
        assert_eq!(tick_economics("MNQU6"), (d("0.25"), d("0.50")));
    }

    #[test]
    fn mgc_contract_has_its_own_tick_size_and_value() {
        assert_eq!(tick_economics("MGCZ6"), (d("0.10"), d("1.00")));
    }

    #[test]
    fn unrecognized_root_falls_back_to_mes_economics() {
        assert_eq!(tick_economics("NQZ6"), (d("0.25"), d("1.25")));
    }
}
