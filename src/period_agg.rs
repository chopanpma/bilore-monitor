//! Incremental 30-min RTH period aggregation with order-flow — the
//! live-tick equivalent of what `period_profiles`/`historical_bars`'
//! `bar_delta` give `bilore-ml-rs` in batch. Needed because
//! `bilore_core::bar_builder::BarBuilder` only tracks high/low/close (no
//! open, no volume, no aggressor side) — not enough for
//! `bilore_ml_rs::predict::LivePeriodInput`'s `prev_bullish`/`prev_range`/
//! `prev_volume`/`prev_delta_ratio` features.

use bilore_core::order_flow::{Side, Tick, TickRuleClassifier};
use rust_decimal::Decimal;
use tpo_builder::profile::SessionConfig;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeriodBar {
    pub period_idx: usize, // 0-based: 'A' = 0
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: i64,
    pub up_vol: i64,
    pub down_vol: i64,
}

struct InProgress {
    period: char,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
    volume: i64,
    up_vol: i64,
    down_vol: i64,
}

pub struct PeriodAggregator {
    config: SessionConfig,
    classifier: TickRuleClassifier,
    current: Option<InProgress>,
}

fn period_idx(period: char) -> usize {
    (period as u8 - b'A') as usize
}

impl PeriodAggregator {
    pub fn new(config: SessionConfig) -> Self {
        Self { config, classifier: TickRuleClassifier::new(), current: None }
    }

    /// Feed one tick. Returns the just-completed `PeriodBar` when this tick
    /// belongs to a new period; ticks outside the session (e.g. overnight)
    /// are ignored entirely, matching v1's RTH-only period model.
    pub fn on_tick(&mut self, tick: &Tick) -> Option<PeriodBar> {
        let period = self.config.period_for(tick.ts)?;
        let side = self.classifier.classify(tick.price);
        let (up, down) = match side {
            Side::Buy => (tick.size, 0),
            Side::Sell => (0, tick.size),
        };

        match &mut self.current {
            Some(cur) if cur.period == period => {
                cur.high = cur.high.max(tick.price);
                cur.low = cur.low.min(tick.price);
                cur.close = tick.price;
                cur.volume += tick.size;
                cur.up_vol += up;
                cur.down_vol += down;
                None
            }
            _ => {
                let finished = self.current.take().map(|cur| PeriodBar {
                    period_idx: period_idx(cur.period),
                    open: cur.open,
                    high: cur.high,
                    low: cur.low,
                    close: cur.close,
                    volume: cur.volume,
                    up_vol: cur.up_vol,
                    down_vol: cur.down_vol,
                });
                self.current = Some(InProgress {
                    period,
                    open: tick.price,
                    high: tick.price,
                    low: tick.price,
                    close: tick.price,
                    volume: tick.size,
                    up_vol: up,
                    down_vol: down,
                });
                finished
            }
        }
    }

    /// The period currently forming (not yet complete), if any — its
    /// `period_idx` and current `close`/`high`/`low` are what a live signal
    /// check needs for "the period happening right now."
    pub fn current_period_idx(&self) -> Option<usize> {
        self.current.as_ref().map(|c| period_idx(c.period))
    }

    pub fn reset(&mut self) {
        self.classifier = TickRuleClassifier::new();
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tpo_builder::profile::rth_session_config;

    // RTH starts 13:30 UTC (08:30 CT). ts(m) = 13:30 UTC + m minutes.
    fn ts(minute_offset: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 8, 13, 30, 0).unwrap() + chrono::Duration::minutes(minute_offset)
    }

    fn tick(minute_offset: i64, price: &str, size: i64) -> Tick {
        Tick { ts: ts(minute_offset), price: price.parse().unwrap(), size }
    }

    #[test]
    fn first_tick_starts_period_a_with_no_completed_bar_yet() {
        let mut agg = PeriodAggregator::new(rth_session_config());
        assert_eq!(agg.on_tick(&tick(0, "5000", 10)), None);
        assert_eq!(agg.current_period_idx(), Some(0));
    }

    #[test]
    fn ticks_within_the_same_period_accumulate_ohlv() {
        let mut agg = PeriodAggregator::new(rth_session_config());
        agg.on_tick(&tick(0, "5000", 10));
        agg.on_tick(&tick(5, "5010", 5));
        agg.on_tick(&tick(10, "4990", 20));
        // Still period A (0-30 min in) — crossing into period B (minute 30) finalizes it.
        let finished = agg.on_tick(&tick(30, "5005", 1)).unwrap();
        assert_eq!(finished.period_idx, 0);
        assert_eq!(finished.open, "5000".parse().unwrap());
        assert_eq!(finished.high, "5010".parse().unwrap());
        assert_eq!(finished.low, "4990".parse().unwrap());
        assert_eq!(finished.close, "4990".parse().unwrap());
        assert_eq!(finished.volume, 35);
    }

    #[test]
    fn up_and_down_volume_follow_the_tick_rule() {
        let mut agg = PeriodAggregator::new(rth_session_config());
        agg.on_tick(&tick(0, "5000", 10)); // first tick always Buy
        agg.on_tick(&tick(1, "5010", 5)); // price up -> Buy
        agg.on_tick(&tick(2, "5000", 7)); // price down -> Sell
        let finished = agg.on_tick(&tick(30, "5005", 1)).unwrap();
        assert_eq!(finished.up_vol, 15); // 10 + 5
        assert_eq!(finished.down_vol, 7);
    }

    #[test]
    fn ticks_outside_rth_are_ignored() {
        let mut agg = PeriodAggregator::new(rth_session_config());
        let overnight = Tick { ts: Utc.with_ymd_and_hms(2026, 9, 8, 2, 0, 0).unwrap(), price: "5000".parse().unwrap(), size: 5 };
        assert_eq!(agg.on_tick(&overnight), None);
        assert_eq!(agg.current_period_idx(), None);
    }

    #[test]
    fn reset_clears_in_progress_state() {
        let mut agg = PeriodAggregator::new(rth_session_config());
        agg.on_tick(&tick(0, "5000", 10));
        assert_eq!(agg.current_period_idx(), Some(0));
        agg.reset();
        assert_eq!(agg.current_period_idx(), None);
    }
}
