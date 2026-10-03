//! `fade-roll-*` (2026-10-02) — pure decisions; `main.rs` wires the I/O.
//!
//! The user's own style, from their 09-28..10-02 fills, with ROLLING
//! levels: fade toward the POC of the last N completed 30-min periods,
//! entering at that window's POC/VAH/VAL/high/low via the live
//! `trade_setup_levels`; stop/target from `poc_anchor::apply_geometry`;
//! fill required; no model gate; no re-take of a stopped level. One
//! strategy per trading window, each with its own scoreboard:
//!
//! | slug | window (CT) | geometry |
//! |---|---|---|
//! | `fade-roll-evening`   | 17:00-02:00 | 48t / 1R |
//! | `fade-roll-overnight` | 02:00-08:30 | 48t / 1R |
//! | `fade-roll-rth`       | 08:30-15:00 | 40t / 0.75R |
//!
//! N = 16 for all three: best or near-best in every window of the 1-min
//! bar backtest (`bilore-ml-rs` `backtest_rolling_levels`); only the
//! overnight window was positive there (+2.3 ticks/trade, both halves).
//! The rolling window is continuous across windows (early RTH sees the
//! last hours of Globex) and resets after a gap of 3+ hours (weekends).

use bilore_core::fade::fade_direction;
use bilore_core::market_structure::Direction;
use bilore_core::strategy;
use bilore_ml_rs::clean_sessions::session_of;
use bilore_ml_rs::poc_anchor::{apply_geometry, ungated_risk, Geometry, TargetRule, VolumeProfile};
use bilore_ml_rs::trade_setup::{trade_setup_levels, SetupResult, TradeSetupConfig};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use rust_decimal::Decimal;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Evening,
    Overnight,
    Rth,
}

/// The trading window `t` falls in (`None` = 15:00-17:00).
pub fn window_of(t: NaiveTime) -> Option<Window> {
    let hm = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
    if t >= hm(17, 0) || t < hm(2, 0) {
        Some(Window::Evening)
    } else if t < hm(8, 30) {
        Some(Window::Overnight)
    } else if t < hm(15, 0) {
        Some(Window::Rth)
    } else {
        None
    }
}

/// The date a trade at `ct` is stored under: the trading date of its
/// Globex session for Evening/Overnight (17:00 belongs to the next trading
/// day), the calendar date for RTH.
pub fn session_date(ct: NaiveDateTime) -> NaiveDate {
    match session_of(ct) {
        Some((date, _)) => date,
        None => ct.date(),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RollConfig {
    pub slug: &'static str,
    pub window: Window,
    pub periods: usize,
    pub geometry: Geometry,
}

pub fn configs() -> [RollConfig; 3] {
    let g = |stop: i64, r: &str| Geometry { stop_ticks: Some(Decimal::from(stop)), target: TargetRule::R(r.parse().unwrap()) };
    [
        RollConfig { slug: strategy::FADE_ROLL_EVENING, window: Window::Evening, periods: 16, geometry: g(48, "1") },
        RollConfig { slug: strategy::FADE_ROLL_OVERNIGHT, window: Window::Overnight, periods: 16, geometry: g(48, "1") },
        RollConfig { slug: strategy::FADE_ROLL_RTH, window: Window::Rth, periods: 16, geometry: g(40, "0.75") },
    ]
}

/// Levels of the last N completed periods.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub poc: Decimal,
    pub vah: Decimal,
    pub val: Decimal,
    pub high: Decimal,
    pub low: Decimal,
}

struct Period {
    vp: VolumeProfile,
    high: Decimal,
    low: Decimal,
}

/// Rolling 30-min periods (aligned to :00/:30) fed tick by tick.
#[derive(Default)]
pub struct RollingLevels {
    done: VecDeque<Period>,
    current: Option<(NaiveDateTime, Period)>,
    last: Option<NaiveDateTime>,
}

const KEEP: usize = 32;

impl RollingLevels {
    pub fn on_tick(&mut self, ct: NaiveDateTime, price: Decimal, size: u64) {
        if self.last.is_some_and(|l| (ct - l).num_hours() >= 3) {
            self.done.clear();
            self.current = None;
        }
        self.last = Some(ct);
        let start = ct.with_minute(if ct.minute() < 30 { 0 } else { 30 }).and_then(|t| t.with_second(0)).unwrap_or(ct);
        let start = start.with_nanosecond(0).unwrap_or(start);
        if self.current.as_ref().is_some_and(|(s, _)| *s != start) {
            if let Some((_, p)) = self.current.take() {
                self.done.push_back(p);
                while self.done.len() > KEEP {
                    self.done.pop_front();
                }
            }
        }
        let (_, p) = self.current.get_or_insert_with(|| (start, Period { vp: VolumeProfile::default(), high: price, low: price }));
        p.vp.add(price, size);
        p.high = p.high.max(price);
        p.low = p.low.min(price);
    }

    /// Levels of the last `n` completed periods; `None` until there are `n`.
    pub fn snapshot(&self, n: usize) -> Option<Snapshot> {
        if n == 0 || self.done.len() < n {
            return None;
        }
        let mut vp = VolumeProfile::default();
        let (mut high, mut low) = (Decimal::MIN, Decimal::MAX);
        for p in self.done.iter().rev().take(n) {
            for (price, v) in p.vp.levels() {
                vp.add(price, v);
            }
            high = high.max(p.high);
            low = low.min(p.low);
        }
        let (val, vah) = vp.value_area()?;
        Some(Snapshot { poc: vp.poc()?, vah, val, high, low })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RollSetup {
    pub direction: Direction,
    pub anchor: String,
    pub entry: Decimal,
    pub stop: Decimal,
    pub target: Decimal,
}

/// The setup to arm at `price` from `snap`, or why there is none.
pub fn roll_setup(
    price: Decimal,
    snap: &Snapshot,
    stopped: &[(Direction, Decimal)],
    geometry: Geometry,
    cfg: &TradeSetupConfig,
) -> Result<RollSetup, String> {
    let direction = fade_direction(price, snap.poc);
    let levels = [
        ("POC".to_string(), snap.poc),
        ("VAH".to_string(), snap.vah),
        ("VAL".to_string(), snap.val),
        ("Roll High".to_string(), snap.high),
        ("Roll Low".to_string(), snap.low),
    ];
    let s = match trade_setup_levels(direction, price, &levels, &ungated_risk(direction), cfg) {
        SetupResult::Setup(s) => s,
        SetupResult::NoSetup(reason) => return Err(reason),
    };
    if stopped.contains(&(direction, s.entry_price)) {
        return Err(format!("{direction:?} at {} was stopped out earlier this session", s.entry_price));
    }
    let (stop, target) =
        apply_geometry(geometry, direction, s.entry_price, s.stop, s.targets.first().map(|t| t.price), cfg.tick_size);
    Ok(RollSetup { direction, anchor: s.anchor_lbl, entry: s.entry_price, stop, target: target.ok_or("no target")? })
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

    fn at(date: &str, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap().and_time(t(h, m))
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn windows_split_the_day_at_17_02_08_30_and_15() {
        assert_eq!(window_of(t(17, 0)), Some(Window::Evening));
        assert_eq!(window_of(t(1, 59)), Some(Window::Evening));
        assert_eq!(window_of(t(2, 0)), Some(Window::Overnight));
        assert_eq!(window_of(t(8, 29)), Some(Window::Overnight));
        assert_eq!(window_of(t(8, 30)), Some(Window::Rth));
        assert_eq!(window_of(t(14, 59)), Some(Window::Rth));
        assert_eq!(window_of(t(15, 0)), None);
        assert_eq!(window_of(t(16, 59)), None);
    }

    #[test]
    fn evening_trades_are_stored_under_the_next_trading_day() {
        assert_eq!(session_date(at("2026-10-01", 20, 0)), day("2026-10-02"));
        assert_eq!(session_date(at("2026-10-02", 1, 0)), day("2026-10-02"));
        assert_eq!(session_date(at("2026-10-02", 18, 0)), day("2026-10-05"), "Friday evening -> Monday");
        assert_eq!(session_date(at("2026-10-02", 10, 0)), day("2026-10-02"), "RTH");
    }

    fn feed(r: &mut RollingLevels, date: &str, h: u32, m: u32, price: &str, size: u64) {
        r.on_tick(at(date, h, m), d(price), size);
    }

    #[test]
    fn only_completed_periods_count() {
        let mut r = RollingLevels::default();
        feed(&mut r, "2026-10-01", 20, 5, "100.00", 10);
        assert_eq!(r.snapshot(1), None, "the 20:00 period is still open");
        feed(&mut r, "2026-10-01", 20, 31, "101.00", 1); // 20:00 period completes
        let s = r.snapshot(1).unwrap();
        assert_eq!((s.poc, s.high, s.low), (d("100.00"), d("100.00"), d("100.00")));
        assert_eq!(r.snapshot(2), None, "only one completed period so far");
    }

    #[test]
    fn the_snapshot_merges_the_last_n_periods_with_their_high_and_low() {
        let mut r = RollingLevels::default();
        feed(&mut r, "2026-10-01", 20, 5, "100.00", 10);
        feed(&mut r, "2026-10-01", 20, 10, "99.00", 1);
        feed(&mut r, "2026-10-01", 20, 35, "101.00", 30);
        feed(&mut r, "2026-10-01", 21, 2, "102.00", 1); // completes 20:30
        let s = r.snapshot(2).unwrap();
        assert_eq!((s.poc, s.high, s.low), (d("101.00"), d("101.00"), d("99.00")));
        let last_only = r.snapshot(1).unwrap();
        assert_eq!((last_only.poc, last_only.low), (d("101.00"), d("101.00")));
    }

    #[test]
    fn a_gap_of_three_hours_or_more_resets_the_window() {
        let mut r = RollingLevels::default();
        feed(&mut r, "2026-10-02", 15, 55, "100.00", 10);
        feed(&mut r, "2026-10-02", 16, 0, "100.25", 1); // would complete 15:30
        assert!(r.snapshot(1).is_some());
        feed(&mut r, "2026-10-04", 17, 0, "105.00", 1); // Sunday reopen
        assert_eq!(r.snapshot(1), None, "weekend gap starts a fresh window");
    }

    fn snap() -> Snapshot {
        Snapshot { poc: d("7732.50"), vah: d("7745.00"), val: d("7722.75"), high: d("7760.00"), low: d("7710.00") }
    }

    fn g48() -> Geometry {
        Geometry { stop_ticks: Some(d("48")), target: TargetRule::R(d("1")) }
    }

    #[test]
    fn above_the_rolling_poc_it_sells_at_the_nearest_level_above() {
        let s = roll_setup(d("7740.00"), &snap(), &[], g48(), &TradeSetupConfig::default()).unwrap();
        assert_eq!((s.direction, s.anchor.as_str()), (Direction::Short, "VAH"));
        assert_eq!((s.entry, s.stop, s.target), (d("7746.50"), d("7758.50"), d("7734.50")));
    }

    #[test]
    fn above_the_value_area_the_rolling_high_is_the_entry() {
        let s = roll_setup(d("7755.00"), &snap(), &[], g48(), &TradeSetupConfig::default()).unwrap();
        assert_eq!((s.direction, s.anchor.as_str()), (Direction::Short, "Roll High"));
        assert_eq!(s.entry, d("7761.50"));
    }

    #[test]
    fn a_stopped_level_is_not_retaken_in_the_same_direction() {
        let stopped = vec![(Direction::Short, d("7746.50"))];
        assert!(roll_setup(d("7740.00"), &snap(), &stopped, g48(), &TradeSetupConfig::default()).is_err());
    }

    #[test]
    fn the_three_strategies_use_16_periods_and_their_geometry() {
        let c = configs();
        assert!(c.iter().all(|c| c.periods == 16));
        assert_eq!(c[1].window, Window::Overnight);
        assert_eq!(c[2].geometry, Geometry { stop_ticks: Some(d("40")), target: TargetRule::R(d("0.75")) });
    }
}
