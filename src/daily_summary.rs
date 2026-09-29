//! End-of-day shadow-trade summary for Telegram (2026-09-28, user request)
//! — pure: when it's due, and what it says. `main.rs` does the DB reads
//! and the send.
//!
//! Sent once per weekday right after the RTH close (15:00–16:00 CT window,
//! chrono-tz so it survives the DST switch). Per strategy: today's trades
//! by outcome + P&L, then the cumulative go-live gate line — computed with
//! `bilore_backtest::gate`, the same code as the `bilore-backtest-gate`
//! report, so the two can never disagree.

use bilore_core::shadow_trader::{TrustConfig, TrustMetrics};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc, Weekday};
use chrono_tz::America::Chicago;
use rust_decimal::prelude::ToPrimitive;
use std::collections::BTreeMap;
use std::fmt::Write;

/// One of today's `shadow_trades_v2` rows, as read from the DB.
#[derive(Debug, Clone)]
pub struct TodayRow {
    pub strategy: String,
    pub outcome: String,
    pub pnl_ticks: Option<f64>,
    pub pnl_dollars: Option<f64>,
}

/// `Some(today_ct)` when the daily summary should go out now: a Mon–Fri
/// CT date, 15:00 ≤ time < 16:00 CT, and not already sent for that date.
pub fn summary_due(now: DateTime<Utc>, last_sent: Option<NaiveDate>) -> Option<NaiveDate> {
    let ct = now.with_timezone(&Chicago);
    let today = ct.date_naive();
    let weekday = !matches!(ct.weekday(), Weekday::Sat | Weekday::Sun);
    let hm = |h| NaiveTime::from_hms_opt(h, 0, 0).unwrap();
    let in_window = ct.time() >= hm(15) && ct.time() < hm(16);
    (weekday && in_window && last_sent != Some(today)).then_some(today)
}

pub fn summary_message(
    date: NaiveDate,
    strategies: &[&str],
    today: &[TodayRow],
    gates: &BTreeMap<String, TrustMetrics>,
    cfg: &TrustConfig,
) -> String {
    let f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);
    let mut out = format!("📊 <b>[V2] DAILY SUMMARY — {date}</b>\n");
    for &slug in strategies {
        let rows: Vec<&TodayRow> = today.iter().filter(|r| r.strategy == slug).collect();
        let count = |o: &[&str]| rows.iter().filter(|r| o.contains(&r.outcome.as_str())).count();
        let _ = write!(out, "\n<b>{slug}</b>\n");
        if rows.is_empty() {
            out.push_str("Today : no trades\n");
        } else {
            let resolved = rows.iter().filter(|r| r.outcome == "won" || r.outcome == "lost");
            let (ticks, dollars) = resolved.fold((0.0, 0.0), |(t, d), r| {
                (t + r.pnl_ticks.unwrap_or(0.0), d + r.pnl_dollars.unwrap_or(0.0))
            });
            let _ = writeln!(
                out,
                "Today : {} — ✅{} ❌{} ⏱{} 🚫{} open {} · {ticks:+.1}t ${dollars:+.2}",
                rows.len(),
                count(&["won"]),
                count(&["lost"]),
                count(&["expired"]),
                count(&["invalidated"]),
                count(&["pending", "entered"]),
            );
        }
        if let Some(g) = gates.get(slug) {
            let _ = writeln!(
                out,
                "Total : {} ({}W/{}L) {:.0}% · exp {:+.1}t · ${:+.2} · {}",
                g.total,
                g.wins,
                g.losses,
                f(g.win_rate) * 100.0,
                f(g.expectancy),
                f(g.total_pnl_dollars),
                match (g.meets_threshold, g.needed) {
                    (true, _) => "✅ LIVE-ready".to_string(),
                    (false, 0) => "❌ not live — below the bar".to_string(),
                    (false, n) => format!("need {n} more · ❌ not live"),
                },
            );
        }
    }
    let _ = write!(
        out,
        "\n<i>Gate: {}+ trades, win ≥ {:.0}%, exp ≥ {:.1}t, P&amp;L ≥ ${:.0}</i>",
        cfg.min_trades,
        f(cfg.required_win_rate) * 100.0,
        f(cfg.required_expectancy),
        f(cfg.required_earnings),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilore_backtest::gate::{compute_gates, group_by_strategy, GateRow, TRUST_CFG};
    use chrono::TimeZone;
    use chrono_tz::America::Chicago;

    fn ct(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Chicago.with_ymd_and_hms(y, m, d, h, min, 0).unwrap().with_timezone(&Utc)
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // 2026-09-28 is a Monday (CDT).

    #[test]
    fn due_right_at_the_rth_close_on_a_weekday() {
        assert_eq!(summary_due(ct(2026, 9, 28, 15, 0), None), Some(date(2026, 9, 28)));
    }

    #[test]
    fn not_due_before_the_close() {
        assert_eq!(summary_due(ct(2026, 9, 28, 14, 59), None), None);
    }

    #[test]
    fn not_due_twice_the_same_day() {
        assert_eq!(summary_due(ct(2026, 9, 28, 15, 1), Some(date(2026, 9, 28))), None);
    }

    #[test]
    fn a_new_day_is_due_again_after_yesterdays_was_sent() {
        assert_eq!(summary_due(ct(2026, 9, 29, 15, 0), Some(date(2026, 9, 28))), Some(date(2026, 9, 29)));
    }

    #[test]
    fn not_due_after_the_one_hour_window() {
        // A restart late in the evening shouldn't send a stale summary.
        assert_eq!(summary_due(ct(2026, 9, 28, 16, 0), None), None);
    }

    #[test]
    fn not_due_on_weekends() {
        assert_eq!(summary_due(ct(2026, 10, 3, 15, 10), None), None); // Saturday
        assert_eq!(summary_due(ct(2026, 10, 4, 15, 10), None), None); // Sunday
    }

    #[test]
    fn uses_chicago_wall_clock_after_dst_ends() {
        // 2026-12-01 is CST (UTC-6): 15:00 CT = 21:00 UTC.
        assert_eq!(summary_due(ct(2026, 12, 1, 15, 0), None), Some(date(2026, 12, 1)));
        assert_eq!(summary_due(ct(2026, 12, 1, 14, 30), None), None);
    }

    fn today(strategy: &str, outcome: &str, ticks: Option<f64>, dollars: Option<f64>) -> TodayRow {
        TodayRow { strategy: strategy.into(), outcome: outcome.into(), pnl_ticks: ticks, pnl_dollars: dollars }
    }

    fn gates(rows: &[(&str, &str, f64, f64)]) -> BTreeMap<String, TrustMetrics> {
        let rows: Vec<GateRow> = rows
            .iter()
            .map(|(s, o, t, d)| GateRow { strategy: s.to_string(), outcome: o.to_string(), pnl_ticks: Some(*t), pnl_dollars: Some(*d) })
            .collect();
        compute_gates(&group_by_strategy(&rows), &STRATS, &TRUST_CFG)
    }

    const STRATS: [&str; 2] = ["ml-model", "fade-poc-fill"];

    fn message() -> String {
        let today = vec![
            today("ml-model", "won", Some(32.0), Some(40.0)),
            today("ml-model", "lost", Some(-16.0), Some(-20.0)),
            today("ml-model", "invalidated", None, None),
            today("ml-model", "entered", None, None),
        ];
        let g = gates(&[
            ("ml-model", "won", 32.0, 40.0),
            ("ml-model", "lost", -16.0, -20.0),
            ("ml-model", "lost", -16.0, -20.0),
        ]);
        summary_message(date(2026, 9, 28), &STRATS, &today, &g, &TRUST_CFG)
    }

    #[test]
    fn header_names_the_date() {
        assert!(message().contains("[V2] DAILY SUMMARY — 2026-09-28"), "{}", message());
    }

    #[test]
    fn today_line_counts_each_outcome_and_sums_resolved_pnl() {
        let m = message();
        assert!(m.contains("<b>ml-model</b>"), "{m}");
        assert!(m.contains("Today : 4 — ✅1 ❌1 ⏱0 🚫1 open 1 · +16.0t $+20.00"), "{m}");
    }

    #[test]
    fn total_line_shows_the_cumulative_gate() {
        let m = message();
        assert!(m.contains("Total : 3 (1W/2L) 33% · exp"), "{m}");
        assert!(m.contains("$+0.00"), "{m}");
        assert!(m.contains("need 17 more · ❌ not live"), "{m}");
    }

    #[test]
    fn a_strategy_with_no_trades_today_says_so() {
        let m = message();
        let fade = &m[m.find("<b>fade-poc-fill</b>").expect("fade section")..];
        assert!(fade.contains("Today : no trades"), "{m}");
        assert!(fade.contains("Total : 0 (0W/0L)"), "{m}");
    }

    #[test]
    fn footer_states_the_gate_threshold() {
        assert!(message().contains("Gate: 20+ trades, win ≥ 35%, exp ≥ 5.0t, P&amp;L ≥ $100"), "{}", message());
    }

    #[test]
    fn enough_trades_but_below_the_bar_says_why_not_how_many_more() {
        let losers: Vec<(&str, &str, f64, f64)> = (0..20).map(|_| ("ml-model", "lost", -16.0, -20.0)).collect();
        let m = summary_message(date(2026, 9, 28), &["ml-model"], &[], &gates(&losers), &TRUST_CFG);
        assert!(m.contains("❌ not live — below the bar"), "{m}");
        assert!(!m.contains("need 0 more"), "{m}");
    }
}
