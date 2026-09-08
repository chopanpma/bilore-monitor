//! Streaming wrapper around `bilore-backtest::signal::find_first_signal` —
//! same structure+order_flow confirmation logic already validated in
//! `bilore-backtest`, but re-evaluated incrementally as live bars/ticks
//! arrive instead of batch-replaying a whole historical session at once.
//! Mirrors v1's `rth_monitor.py`/`bilore_session.py` lock semantics: once a
//! signal fires, it stays locked until the structural bias actually flips
//! (not re-fired every bar), matching "lock the first valid setup; unlock
//! only if lean direction flips."

use bilore_backtest::signal::{find_first_signal, GeneratedSignal, SignalConfig};
use bilore_core::confirmation::AlertConfig;
use bilore_core::market_structure::{analyze_structure, structure_direction, Bar};
use bilore_core::order_flow::Tick;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MonitorEvent {
    NewSignal(GeneratedSignal),
    Unlocked,
}

pub struct LiveMonitor {
    bars: Vec<Bar>,
    ticks: Vec<Tick>,
    locked: Option<GeneratedSignal>,
    /// Index into `bars` that a fresh `find_first_signal` scan starts from.
    /// Without this, unlocking would immediately re-find the SAME
    /// already-passed signal on the next bar — `find_first_signal` always
    /// looks for the first directional bias from the start of whatever
    /// slice it's given, which is correct for a one-shot historical replay
    /// but not for a continuously-accumulating live array. Advanced to
    /// `bars.len()` every time a signal locks or unlocks.
    search_from: usize,
    cfg: SignalConfig,
    alert_cfg: AlertConfig,
}

impl LiveMonitor {
    pub fn new(cfg: SignalConfig, alert_cfg: AlertConfig) -> Self {
        Self { bars: Vec::new(), ticks: Vec::new(), locked: None, search_from: 0, cfg, alert_cfg }
    }

    pub fn on_bar(&mut self, bar: Bar) -> Option<MonitorEvent> {
        self.bars.push(bar);
        self.reevaluate()
    }

    pub fn on_tick(&mut self, tick: Tick) -> Option<MonitorEvent> {
        self.ticks.push(tick);
        self.reevaluate()
    }

    fn reevaluate(&mut self) -> Option<MonitorEvent> {
        if let Some(locked) = &self.locked {
            let structure = analyze_structure(&self.bars, None, self.cfg.pivot_n);
            if let Some(dir) = structure_direction(&structure) {
                if dir != locked.direction {
                    self.locked = None;
                    self.search_from = self.bars.len();
                    return Some(MonitorEvent::Unlocked);
                }
            }
            return None;
        }

        if let Some(sig) = find_first_signal(&self.bars[self.search_from..], &self.ticks, &self.cfg, &self.alert_cfg) {
            self.locked = Some(sig);
            self.search_from = self.bars.len();
            return Some(MonitorEvent::NewSignal(sig));
        }
        None
    }

    /// Clear all accumulated state for a new session — call on CT calendar
    /// day rollover (main.rs's job to detect; this module has no clock).
    pub fn reset_for_new_day(&mut self) {
        self.bars.clear();
        self.ticks.clear();
        self.locked = None;
        self.search_from = 0;
    }

    pub fn locked_signal(&self) -> Option<&GeneratedSignal> {
        self.locked.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ts(minute: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + minute * 60, 0).unwrap()
    }

    fn bar(minute: i64, high: &str, low: &str, close: &str) -> Bar {
        Bar { ts: ts(minute), high: high.parse().unwrap(), low: low.parse().unwrap(), close: close.parse().unwrap() }
    }

    fn tick(minute: i64, price: &str, size: i64) -> Tick {
        Tick { ts: ts(minute), price: price.parse().unwrap(), size }
    }

    fn cfg() -> SignalConfig {
        SignalConfig { pivot_n: 1, stop_buffer: "1".parse().unwrap(), target_r_multiple: "2".parse().unwrap(), scan_start_ts: None }
    }

    /// Same bearish shape used throughout bilore-backtest/bilore-replay's
    /// tests: two lower-high/lower-low swing pairs (n=1 pivots), resolving
    /// to LhLl (Bearish/Short) once all 7 bars are seen.
    fn bearish_bars() -> Vec<Bar> {
        vec![
            bar(0, "5100", "5090", "5095"),
            bar(1, "5110", "5080", "5085"),
            bar(2, "5070", "5060", "5065"),
            bar(3, "5090", "5075", "5080"),
            bar(4, "5085", "5030", "5035"),
            bar(5, "5020", "5000", "5005"),
            bar(6, "5010", "5002", "5008"),
        ]
    }

    fn heavy_sell_ticks() -> Vec<Tick> {
        vec![tick(0, "5100", 5), tick(1, "5090", 200), tick(2, "5060", 200), tick(3, "5000", 200)]
    }

    /// Reflection of bearish_bars about 5500 (new_high = 11000 - old_low,
    /// new_low = 11000 - old_high) — a rigorous, not hand-tuned, way to
    /// turn a proven LhLl (Bearish) fixture into a proven HhHl (Bullish)
    /// one: reflection swaps swing highs <-> swing lows, and since the
    /// original swing lows/highs were each strictly decreasing (LhLl),
    /// their reflections are each strictly increasing (HhHl).
    fn bullish_bars() -> Vec<Bar> {
        vec![
            bar(7, "5910", "5900", "5905"),
            bar(8, "5920", "5890", "5915"),
            bar(9, "5940", "5930", "5935"),
            bar(10, "5925", "5910", "5920"),
            bar(11, "5970", "5915", "5965"),
            bar(12, "6000", "5980", "5995"),
            bar(13, "5998", "5990", "5992"),
        ]
    }

    #[test]
    fn no_event_with_too_few_bars() {
        let mut m = LiveMonitor::new(cfg(), AlertConfig::default());
        assert_eq!(m.on_bar(bar(0, "5100", "5090", "5095")), None);
        assert_eq!(m.locked_signal(), None);
    }

    #[test]
    fn emits_new_signal_once_enough_bars_confirm_a_bias() {
        let mut m = LiveMonitor::new(cfg(), AlertConfig::default());
        let mut last_event = None;
        for (i, b) in bearish_bars().into_iter().enumerate() {
            for t in heavy_sell_ticks().into_iter().filter(|t| t.ts <= ts(i as i64)) {
                m.on_tick(t);
            }
            last_event = m.on_bar(b);
        }
        assert!(matches!(last_event, Some(MonitorEvent::NewSignal(_))));
        assert!(m.locked_signal().is_some());
    }

    #[test]
    fn stays_locked_and_emits_nothing_further_while_bias_holds() {
        let mut m = LiveMonitor::new(cfg(), AlertConfig::default());
        for b in bearish_bars() {
            m.on_bar(b);
        }
        let locked_after_first = m.locked_signal().copied();
        assert!(locked_after_first.is_some());

        // One more bar continuing the same bearish structure — should NOT
        // re-fire NewSignal or change the locked signal.
        let event = m.on_bar(bar(7, "5008", "4998", "5000"));
        assert_eq!(event, None);
        assert_eq!(m.locked_signal().copied(), locked_after_first);
    }

    #[test]
    fn unlocks_when_the_bias_flips() {
        let mut m = LiveMonitor::new(cfg(), AlertConfig::default());
        for b in bearish_bars() {
            m.on_bar(b);
        }
        assert!(m.locked_signal().is_some());

        let events: Vec<MonitorEvent> = bullish_bars().into_iter().filter_map(|b| m.on_bar(b)).collect();
        assert!(events.contains(&MonitorEvent::Unlocked), "expected an Unlocked event, got {events:?}");
    }

    #[test]
    fn reset_for_new_day_clears_everything() {
        let mut m = LiveMonitor::new(cfg(), AlertConfig::default());
        for b in bearish_bars() {
            m.on_bar(b);
        }
        assert!(m.locked_signal().is_some());

        m.reset_for_new_day();
        assert_eq!(m.locked_signal(), None);

        // A fresh, too-short bar sequence after reset shouldn't immediately
        // re-signal from stale state.
        assert_eq!(m.on_bar(bar(0, "5100", "5090", "5095")), None);
    }
}
