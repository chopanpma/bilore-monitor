//! Per-symbol volatility gate (bilore-specs SYS-006) — wires
//! `bilore_core::volatility` to the monitor's live ticks and turns block
//! changes into Telegram notices. Pure; `main.rs` does the I/O (loading the
//! baseline and calendar, sending messages).
//!
//! Only NEW setups consult the gate (user, 2026-10-04): a pending setup may
//! still fill and an open position keeps its stop and target.

use bilore_core::volatility::{check, release_block, Baseline, Block, OpenKind, Release, SpikeGuard};
use chrono::{NaiveDateTime, NaiveTime};
use rust_decimal::Decimal;

/// When a block is expected to end, for the Telegram notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Until {
    /// A known Chicago wall-clock time (release window end, open window end).
    At(NaiveDateTime),
    /// A spike: after this many minutes with no new spike.
    CalmMinutes(u32),
}

/// A change worth one Telegram message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Started { block: Block, until: Until },
    Ended { block: Block },
}

#[derive(Debug, Clone, Default)]
pub struct VolGate {
    baseline: Baseline,
    spike: SpikeGuard,
    calendar: Option<Vec<Release>>,
    /// The block last announced, to report changes once.
    active: Option<Block>,
}

/// Same reason for notice purposes: a spike's ratio changing is the same block.
fn same_reason(a: &Block, b: &Block) -> bool {
    match (a, b) {
        (Block::Spike(_), Block::Spike(_)) => true,
        _ => a == b,
    }
}

impl VolGate {
    /// `calendar` is `None` when the news calendar could not be loaded.
    pub fn new(baseline: Baseline, calendar: Option<Vec<Release>>) -> Self {
        Self { baseline, calendar, ..Self::default() }
    }

    /// New day: a fresh baseline; the live 30-second window carries over.
    pub fn set_baseline(&mut self, baseline: Baseline) {
        self.baseline = baseline;
    }

    pub fn set_calendar(&mut self, calendar: Option<Vec<Release>>) {
        self.calendar = calendar;
    }

    /// Feed one live trade (Chicago wall clock).
    pub fn on_tick(&mut self, at: NaiveDateTime, price: Decimal) {
        let normal = self.baseline.normal_for(at);
        self.spike.on_tick(at, price, normal);
    }

    /// `Ok` when a new setup may be created at `now`.
    pub fn may_open(&self, now: NaiveDateTime) -> Result<(), Block> {
        match check(now, self.calendar.as_deref(), &self.spike).block {
            Some(block) => Err(block),
            None => Ok(()),
        }
    }

    pub fn calendar_unavailable(&self) -> bool {
        self.calendar.is_none()
    }

    /// Call on every tick: a notice when a block starts, changes reason, or
    /// ends; `None` while nothing changed (an extended spike included).
    pub fn update(&mut self, now: NaiveDateTime) -> Option<Notice> {
        let current = self.may_open(now).err();
        let notice = match (&self.active, &current) {
            (None, None) => None,
            (Some(prev), Some(cur)) if same_reason(prev, cur) => None,
            (Some(prev), None) => Some(Notice::Ended { block: prev.clone() }),
            (_, Some(cur)) => Some(Notice::Started { block: cur.clone(), until: self.until(now, cur) }),
        };
        self.active = current;
        notice
    }

    fn until(&self, now: NaiveDateTime, block: &Block) -> Until {
        let at = |h, m| Until::At(now.date().and_time(NaiveTime::from_hms_opt(h, m, 0).unwrap()));
        match block {
            Block::Release(_) => self
                .calendar
                .as_deref()
                .and_then(|cal| release_block(now, cal))
                .map(|r| Until::At(r.at + chrono::Duration::minutes(15)))
                .unwrap_or(Until::CalmMinutes(5)),
            Block::Open(OpenKind::Rth) => at(9, 0),
            Block::Open(OpenKind::GlobexReopen) => at(17, 10),
            Block::Spike(_) => Until::CalmMinutes(5),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilore_core::volatility::{OpenKind, RangeSample};
    use chrono::NaiveDate;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// Chicago wall clock; 2026-10-02 Fri, 10-05 Mon.
    fn ct(day: u32, h: u32, m: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(h, m, s).unwrap()
    }

    /// A normal 30-s range of 1.75 everywhere in RTH and Globex.
    fn baseline() -> Baseline {
        let samples: Vec<_> = (1..=6)
            .flat_map(|s| {
                let session = NaiveDate::from_ymd_opt(2026, 9, s).unwrap();
                [ct(5, 10, 0, 0), ct(5, 20, 0, 0)].map(|at| RangeSample { session, at, range: d("1.75") })
            })
            .collect();
        Baseline::build(&samples)
    }

    fn nfp() -> Vec<Release> {
        vec![Release { name: "Non-Farm Employment Change".into(), at: ct(2, 7, 30, 0) }]
    }

    /// Two trades `points` apart within 20 s, ending at `at`.
    fn jump(g: &mut VolGate, at: NaiveDateTime, points: &str) {
        g.on_tick(at - chrono::Duration::seconds(20), d("5000.00"));
        g.on_tick(at, d("5000.00") + d(points));
    }

    #[test]
    fn a_quiet_gate_allows_new_setups() {
        let g = VolGate::new(baseline(), Some(nfp()));
        assert_eq!(g.may_open(ct(5, 10, 30, 0)), Ok(()));
        assert!(!g.calendar_unavailable());
    }

    #[test]
    fn ticks_feed_the_spike_check_with_the_baseline_normal() {
        let mut g = VolGate::new(baseline(), Some(vec![]));
        jump(&mut g, ct(5, 10, 30, 20), "7.25"); // 4.14x of 1.75
        assert!(matches!(g.may_open(ct(5, 10, 30, 20)), Err(Block::Spike(_))));
    }

    #[test]
    fn a_release_block_is_announced_with_its_end_time() {
        let mut g = VolGate::new(baseline(), Some(nfp()));
        assert_eq!(g.update(ct(2, 6, 59, 0)), None);
        assert_eq!(
            g.update(ct(2, 7, 0, 0)),
            Some(Notice::Started {
                block: Block::Release("Non-Farm Employment Change".into()),
                until: Until::At(ct(2, 7, 45, 0)),
            })
        );
    }

    #[test]
    fn a_block_is_announced_once_even_when_extended() {
        let mut g = VolGate::new(baseline(), Some(vec![]));
        jump(&mut g, ct(5, 10, 30, 20), "7.25");
        assert!(matches!(
            g.update(ct(5, 10, 30, 20)),
            Some(Notice::Started { block: Block::Spike(_), until: Until::CalmMinutes(5) })
        ));
        assert_eq!(g.update(ct(5, 10, 31, 0)), None);
        jump(&mut g, ct(5, 10, 32, 20), "8.00"); // a second spike extends the block
        assert_eq!(g.update(ct(5, 10, 32, 20)), None, "no second message for an extension");
        assert_eq!(g.update(ct(5, 10, 37, 19)), None, "still inside 5 calm minutes");
        assert!(matches!(g.update(ct(5, 10, 37, 20)), Some(Notice::Ended { block: Block::Spike(_) })));
    }

    #[test]
    fn the_end_of_a_block_is_announced() {
        let mut g = VolGate::new(baseline(), Some(vec![]));
        assert_eq!(
            g.update(ct(5, 8, 30, 0)),
            Some(Notice::Started { block: Block::Open(OpenKind::Rth), until: Until::At(ct(5, 9, 0, 0)) })
        );
        assert_eq!(g.update(ct(5, 8, 45, 0)), None);
        assert_eq!(g.update(ct(5, 9, 0, 0)), Some(Notice::Ended { block: Block::Open(OpenKind::Rth) }));
        assert_eq!(g.update(ct(5, 9, 1, 0)), None);
    }

    #[test]
    fn when_a_release_ends_inside_a_spike_the_spike_is_announced() {
        let mut g = VolGate::new(baseline(), Some(nfp()));
        g.update(ct(2, 7, 0, 0)); // release block started
        jump(&mut g, ct(2, 7, 44, 30), "8.00");
        assert_eq!(g.update(ct(2, 7, 44, 30)), None, "the release block is still the reason");
        assert!(matches!(
            g.update(ct(2, 7, 45, 0)),
            Some(Notice::Started { block: Block::Spike(_), until: Until::CalmMinutes(5) })
        ));
    }

    #[test]
    fn a_missing_calendar_is_reported_and_entries_continue() {
        let g = VolGate::new(baseline(), None);
        assert!(g.calendar_unavailable());
        assert_eq!(g.may_open(ct(5, 10, 30, 0)), Ok(()));
    }

    #[test]
    fn a_calendar_loaded_later_takes_effect() {
        let mut g = VolGate::new(baseline(), None);
        g.set_calendar(Some(nfp()));
        assert!(!g.calendar_unavailable());
        assert!(matches!(g.may_open(ct(2, 7, 10, 0)), Err(Block::Release(_))));
    }
}
