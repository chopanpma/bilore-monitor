//! Market-data outage watch (MON-015, user request 2026-10-17) — pure;
//! `main.rs` feeds it the last recorded trade and sends the Telegram notices.
//!
//! When no MES trade has been recorded for `SILENCE` while the market is
//! open, report it once; when trades resume, report that once. Silence after
//! a reopen counts from 17:00 CT, so a weekend never reads as an outage.
//! Market hours (America/Chicago wall clock): closed 16:00-17:00 daily and
//! from Friday 16:00 to Sunday 17:00. Holidays are not modelled.
//!
//! Measured 2026-10-17 on MES since 09-30: a real quiet market almost never
//! goes 10 minutes without a trade (RTH never), while the feed dropped for
//! ~16 minutes dozens of times and for whole nights several times.

use chrono::{Datelike, Duration, NaiveDateTime, NaiveTime, Weekday};

/// Silence that counts as an outage while the market is open.
pub const SILENCE: Duration = Duration::minutes(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedNotice {
    /// No trade since `since` (Chicago wall clock), `minutes` so far.
    Down { since: NaiveDateTime, minutes: i64 },
    /// Trades resumed after an outage of `minutes`.
    Back { since: NaiveDateTime, minutes: i64 },
}

/// CME equity futures hours on the Chicago wall clock.
pub fn market_open(ct: NaiveDateTime) -> bool {
    let (t, halt, reopen) = (ct.time(), hm(16, 0), hm(17, 0));
    match ct.weekday() {
        Weekday::Sat => false,
        Weekday::Sun => t >= reopen,
        Weekday::Fri => t < halt,
        _ => t < halt || t >= reopen,
    }
}

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

/// The last 17:00 CT reopen at or before `now` — the start of the open
/// stretch `now` is in.
fn opened_at(now: NaiveDateTime) -> NaiveDateTime {
    let day = if now.time() >= hm(17, 0) { now.date() } else { now.date() - Duration::days(1) };
    day.and_time(hm(17, 0))
}

#[derive(Debug, Clone, Default)]
pub struct FeedWatch {
    /// An outage in progress: since when, and the last trade seen at that point.
    down: Option<(NaiveDateTime, Option<NaiveDateTime>)>,
}

impl FeedWatch {
    /// Call regularly with the Chicago time and the last recorded trade.
    pub fn check(&mut self, now: NaiveDateTime, last_trade: Option<NaiveDateTime>) -> Option<FeedNotice> {
        if !market_open(now) {
            self.down = None; // nothing is expected while closed
            return None;
        }
        if let Some((since, seen)) = self.down {
            if last_trade > seen {
                self.down = None;
                let resumed = last_trade.unwrap_or(now);
                return Some(FeedNotice::Back { since, minutes: (resumed - since).num_minutes() });
            }
            return None;
        }
        let opened = opened_at(now);
        let since = last_trade.map_or(opened, |t| t.max(opened));
        if now - since < SILENCE {
            return None;
        }
        self.down = Some((since, last_trade));
        Some(FeedNotice::Down { since, minutes: (now - since).num_minutes() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    /// 2026-10-13 Tue, 10-16 Fri, 10-17 Sat, 10-18 Sun.
    fn ct(day: u32, h: u32, m: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(h, m, s).unwrap()
    }

    #[test]
    fn market_hours_follow_cme_equity_futures() {
        assert!(market_open(ct(13, 10, 0, 0)));
        assert!(!market_open(ct(13, 16, 30, 0)), "daily halt");
        assert!(market_open(ct(13, 17, 0, 0)));
        assert!(!market_open(ct(16, 16, 30, 0)), "Friday after the close");
        assert!(!market_open(ct(17, 12, 0, 0)), "Saturday");
        assert!(!market_open(ct(18, 16, 59, 0)), "Sunday before the reopen");
        assert!(market_open(ct(18, 17, 0, 0)), "Sunday reopen");
    }

    #[test]
    fn ten_minutes_of_silence_while_open_alerts_once() {
        let mut w = FeedWatch::default();
        let last = Some(ct(13, 10, 0, 0));
        assert_eq!(w.check(ct(13, 10, 9, 59), last), None);
        assert_eq!(w.check(ct(13, 10, 10, 0), last), Some(FeedNotice::Down { since: ct(13, 10, 0, 0), minutes: 10 }));
        assert_eq!(w.check(ct(13, 10, 11, 0), last), None);
        assert_eq!(w.check(ct(13, 10, 20, 0), last), None);
    }

    #[test]
    fn quiet_under_ten_minutes_sends_nothing() {
        let mut w = FeedWatch::default();
        for k in 0..60u32 {
            let now = ct(13, 10, 0, 0) + Duration::minutes(k as i64);
            let last = ct(13, 10, 0, 0) + Duration::minutes((k / 9 * 9) as i64);
            assert_eq!(w.check(now, Some(last)), None, "at +{k} min");
        }
    }

    #[test]
    fn data_back_sends_one_all_clear() {
        let mut w = FeedWatch::default();
        w.check(ct(13, 10, 15, 0), Some(ct(13, 10, 0, 0)));
        let back = Some(ct(13, 10, 25, 0));
        assert_eq!(w.check(ct(13, 10, 25, 5), back), Some(FeedNotice::Back { since: ct(13, 10, 0, 0), minutes: 25 }));
        assert_eq!(w.check(ct(13, 10, 25, 10), back), None);
    }

    #[test]
    fn the_reopen_gets_a_grace_period() {
        let mut w = FeedWatch::default();
        let last = Some(ct(16, 15, 59, 0));
        assert_eq!(w.check(ct(18, 17, 5, 0), last), None);
        assert_eq!(w.check(ct(18, 17, 10, 0), last), Some(FeedNotice::Down { since: ct(18, 17, 0, 0), minutes: 10 }));
    }

    #[test]
    fn silence_while_closed_never_alerts() {
        let mut w = FeedWatch::default();
        let last = Some(ct(16, 15, 59, 0));
        assert_eq!(w.check(ct(17, 12, 0, 0), last), None);
        assert_eq!(w.check(ct(18, 16, 59, 0), last), None);
    }
}
