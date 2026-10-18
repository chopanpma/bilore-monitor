//! The 2026-10-17 sweep's developing-POC / today's-levels fades (MON-017) —
//! pure; `main.rs` wires the I/O. Every rule is a `Config` over
//! `bilore_ml_rs::poc_anchor`'s `TodayTracker` + `arm` (ML-007), the code
//! `backtest_profile_sweep` runs, so the live rule is the backtested one.
//! Evidence and no-re-take flags: bilore-ml-rs
//! docs/findings/2026-10-17-profile-sweep.md ("Re-validation").

use bilore_core::fade::{fade_direction, fade_trust};
use bilore_core::market_structure::Direction;
use bilore_core::shadow_trader::ShadowTrade;
use bilore_core::strategy;
use bilore_ml_rs::poc_anchor::{self, Geometry, Profile3, Rule, TargetRule, Today, Variant};
use bilore_ml_rs::trade_setup::TradeSetupConfig;
use rust_decimal::Decimal;

/// What the stored entry trust measures the extension from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustFrom {
    /// The POC the rule fades.
    FadedPoc,
    /// The nearest of today's levels (overnight/IB high/low, developing POC).
    NearestTodayLevel,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub slug: &'static str,
    pub va_pct: u64,
    pub rule: Rule,
    pub trust: TrustFrom,
}

/// A setup ready to store.
#[derive(Debug, Clone)]
pub struct Armed {
    pub trade: ShadowTrade,
    pub anchor: String,
    pub faded_poc: Decimal,
}

fn rule(variant: Variant, stop_ticks: i64, r: &str, no_retake: bool) -> Rule {
    let geometry = Geometry { stop_ticks: Some(Decimal::from(stop_ticks)), target: TargetRule::R(r.parse().unwrap()) };
    Rule { variant, geometry, no_retake }
}

pub fn configs() -> [Config; 5] {
    use TrustFrom::*;
    [
        Config { slug: strategy::FADE_DEVPOC_SW, va_pct: 60, rule: rule(Variant::CStability, 120, "1", true), trust: FadedPoc },
        Config { slug: strategy::FADE_DEVPOC_ANCHOR_2R, va_pct: 60, rule: rule(Variant::EC, 40, "2", false), trust: NearestTodayLevel },
        Config { slug: strategy::FADE_DEVPOC_LIVE_VA, va_pct: 70, rule: rule(Variant::EC, 120, "0.75", false), trust: FadedPoc },
        Config { slug: strategy::FADE_DEVPOC_0930, va_pct: 60, rule: rule(Variant::CTime, 120, "1", true), trust: FadedPoc },
        Config { slug: strategy::FADE_TODAYLVL_075R, va_pct: 60, rule: rule(Variant::E, 60, "0.75", false), trust: NearestTodayLevel },
    ]
}

/// The POC the rule fades right now (prior until a handoff variant is clear).
pub fn faded_poc(cfg: &Config, prior: Profile3, today: &Today, clear: bool) -> Decimal {
    poc_anchor::active_profile(cfg.rule.variant, prior, today, clear).poc
}

/// The setup to arm at `price`, or why there is none.
pub fn setup(
    cfg: &Config,
    prior: Profile3,
    today: &Today,
    clear: bool,
    price: Decimal,
    stopped: &[(Direction, Decimal)],
    setup_cfg: &TradeSetupConfig,
) -> Result<Armed, String> {
    let poc = faded_poc(cfg, prior, today, clear);
    let direction = fade_direction(price, poc);
    let (trade, anchor) =
        poc_anchor::arm(&cfg.rule, prior, today, clear, direction, price, &poc_anchor::ungated_risk(direction), stopped, setup_cfg)?;
    Ok(Armed { trade, anchor, faded_poc: poc })
}

/// Registry entry trust: 0.5 + 0.5 x clamp01(distance / stop distance).
pub fn trust(cfg: &Config, price: Decimal, today: &Today, armed: &Armed) -> f64 {
    let from = match cfg.trust {
        TrustFrom::FadedPoc => Some(armed.faded_poc),
        TrustFrom::NearestTodayLevel => [today.on_high, today.on_low, today.ib_high, today.ib_low, today.developing.map(|p| p.poc)]
            .into_iter()
            .flatten()
            .min_by_key(|l| (price - l).abs()),
    };
    match from {
        Some(level) => fade_trust(price, level, (armed.trade.entry_lo - armed.trade.stop).abs()),
        None => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn by_slug(slug: &str) -> Config {
        *configs().iter().find(|c| c.slug == slug).unwrap()
    }

    // 09-29 RTH profile at 60%-ish; today's levels as in poc_anchor's tests.
    fn prior() -> Profile3 {
        Profile3 { poc: d("7732.50"), vah: d("7745.00"), val: d("7722.75") }
    }

    fn today() -> Today {
        Today {
            on_high: Some(d("7760.00")),
            on_low: Some(d("7737.75")),
            ib_high: Some(d("7768.00")),
            ib_low: Some(d("7748.00")),
            developing: Some(Profile3 { poc: d("7770.00"), vah: d("7776.00"), val: d("7764.00") }),
        }
    }

    #[test]
    fn the_five_profile_fades_are_the_sweep_winners() {
        let row = |c: &Config| (c.slug, c.rule.variant, c.va_pct, c.rule.geometry.stop_ticks, c.rule.geometry.target, c.rule.no_retake);
        let r = |x: &str| TargetRule::R(d(x));
        let got: Vec<_> = configs().iter().map(row).collect();
        assert_eq!(
            got,
            vec![
                (strategy::FADE_DEVPOC_SW, Variant::CStability, 60, Some(d("120")), r("1"), true),
                (strategy::FADE_DEVPOC_ANCHOR_2R, Variant::EC, 60, Some(d("40")), r("2"), false),
                (strategy::FADE_DEVPOC_LIVE_VA, Variant::EC, 70, Some(d("120")), r("0.75"), false),
                (strategy::FADE_DEVPOC_0930, Variant::CTime, 60, Some(d("120")), r("1"), true),
                (strategy::FADE_TODAYLVL_075R, Variant::E, 60, Some(d("60")), r("0.75"), false),
            ]
        );
        assert_eq!(by_slug(strategy::FADE_DEVPOC_LIVE_VA).trust, TrustFrom::FadedPoc);
        assert_eq!(by_slug(strategy::FADE_DEVPOC_ANCHOR_2R).trust, TrustFrom::NearestTodayLevel);
        assert_eq!(by_slug(strategy::FADE_TODAYLVL_075R).trust, TrustFrom::NearestTodayLevel);
    }

    #[test]
    fn a_stable_poc_fade_leans_on_the_prior_poc_until_clear_then_the_developing_one() {
        let sw = by_slug(strategy::FADE_DEVPOC_SW);
        assert_eq!(faded_poc(&sw, prior(), &today(), false), d("7732.50"));
        assert_eq!(faded_poc(&sw, prior(), &today(), true), d("7770.00"));
        let cfg = TradeSetupConfig::default();
        let before = setup(&sw, prior(), &today(), false, d("7740.00"), &[], &cfg).unwrap();
        assert_eq!((before.trade.direction, before.faded_poc), (Direction::Short, d("7732.50")));
        let after = setup(&sw, prior(), &today(), true, d("7766.00"), &[], &cfg).unwrap();
        assert_eq!((after.trade.direction, after.faded_poc), (Direction::Long, d("7770.00")));
        assert_eq!(after.trade.entry_lo - after.trade.stop, d("30.00"), "120-tick stop");
    }

    #[test]
    fn todays_levels_add_entry_anchors() {
        let cfg = TradeSetupConfig::default();
        let e = by_slug(strategy::FADE_TODAYLVL_075R);
        let armed = setup(&e, prior(), &today(), false, d("7750.00"), &[], &cfg).unwrap();
        assert_eq!(armed.trade.direction, Direction::Short, "E keeps the prior POC's direction");
        assert!(["ON High", "IB High", "Dev POC"].contains(&armed.anchor.as_str()), "{}", armed.anchor);
    }

    #[test]
    fn profile_fade_trust_follows_the_registry() {
        let cfg = TradeSetupConfig::default();
        let live_va = by_slug(strategy::FADE_DEVPOC_LIVE_VA);
        let mut armed = setup(&live_va, prior(), &today(), true, d("7766.00"), &[], &cfg).unwrap();
        assert_eq!(armed.faded_poc, d("7770.00"));
        armed.trade.stop = armed.trade.entry_lo - d("10.00"); // a 10-point stop for round numbers
        assert_eq!(trust(&live_va, d("7766.00"), &today(), &armed), 0.7, "4 of 10 points from the faded POC");
        let anchor_2r = by_slug(strategy::FADE_DEVPOC_ANCHOR_2R);
        assert_eq!(trust(&anchor_2r, d("7766.00"), &today(), &armed), 0.6, "2 of 10 points from IB high 7768");
    }
}
