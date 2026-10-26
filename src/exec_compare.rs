//! Shadow vs paper/live, for the daily summary (MON-026, user request
//! 2026-10-26) — pure; `main.rs` reads the pairs and sends the message.
//!
//! Every executed order carries its shadow trade's id (`live_orders.trade_id`),
//! so each one pairs with the rule's own outcome. Per strategy and mode the
//! summary shows today's pairs and the running total, and flags a divergence
//! once there are `MIN_PAIRS` compared trades and either the average $ gap
//! (real − shadow) is at least `GAP_USD` per trade with |t| ≥ `T_STAT`, or
//! more than `MAX_DISAGREE` of them disagree (another outcome, a missed fill,
//! an extra fill).

use chrono::NaiveDate;

/// Compared trades before a divergence can be called.
pub const MIN_PAIRS: usize = 5;
/// Average gap per trade that matters: 2 MES ticks on 1 contract.
pub const GAP_USD: f64 = 2.5;
/// |mean gap / standard error| for the gap to count as consistent.
pub const T_STAT: f64 = 2.0;
/// Share of disagreeing trades that counts as a divergence.
pub const MAX_DISAGREE: f64 = 0.20;

/// One `live_orders` row joined to its shadow trade.
#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    pub mode: String,
    pub strategy: String,
    pub trading_date: NaiveDate,
    /// live_orders.status: placing | working | filled | closed | cancelled
    pub status: String,
    /// live_orders.exit_reason: target | stop | close | cancel
    pub exit_reason: Option<String>,
    pub realized_usd: Option<f64>,
    /// shadow_trades_v2.outcome
    pub shadow_outcome: String,
    pub shadow_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Compared {
    /// Both traded and both finished: `same` when the exits agree.
    Both { same: bool, real: f64, shadow: f64 },
    /// The shadow trade traded; the real order never filled.
    Missed { shadow: f64 },
    /// The real order filled; the shadow trade never entered.
    Extra { real: f64 },
}

impl Compared {
    fn gap(&self) -> f64 {
        match *self {
            Compared::Both { real, shadow, .. } => real - shadow,
            Compared::Missed { shadow } => -shadow,
            Compared::Extra { real } => real,
        }
    }
    fn agrees(&self) -> bool {
        matches!(self, Compared::Both { same: true, .. })
    }
}

/// `None` while either side is still open, or when neither traded (an
/// invalidated shadow trade whose order was cancelled unfilled).
pub fn compare(p: &Pair) -> Option<Compared> {
    let shadow_traded = matches!(p.shadow_outcome.as_str(), "won" | "lost" | "expired");
    let shadow_open = matches!(p.shadow_outcome.as_str(), "pending" | "entered");
    let real_done = p.status == "closed";
    let real_unfilled = p.status == "cancelled" && p.realized_usd.is_none();
    let shadow_usd = p.shadow_usd.unwrap_or(0.0);
    if real_done && shadow_traded {
        let same = matches!(
            (p.exit_reason.as_deref(), p.shadow_outcome.as_str()),
            (Some("target"), "won") | (Some("stop"), "lost") | (Some("close"), "expired")
        );
        return Some(Compared::Both { same, real: p.realized_usd.unwrap_or(0.0), shadow: shadow_usd });
    }
    if real_unfilled && shadow_traded {
        return Some(Compared::Missed { shadow: shadow_usd });
    }
    if real_done && !shadow_traded && !shadow_open {
        return Some(Compared::Extra { real: p.realized_usd.unwrap_or(0.0) });
    }
    None
}

/// Totals over a set of compared trades.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tally {
    pub n: usize,
    pub same: usize,
    pub other_outcome: usize,
    pub missed: usize,
    pub extra: usize,
    pub real_usd: f64,
    pub shadow_usd: f64,
    /// mean gap / standard error (0 with fewer than 2 trades or no spread).
    pub t: f64,
}

impl Tally {
    pub fn of(items: &[Compared]) -> Tally {
        let n = items.len();
        let gaps: Vec<f64> = items.iter().map(Compared::gap).collect();
        let mean = if n > 0 { gaps.iter().sum::<f64>() / n as f64 } else { 0.0 };
        let sd = if n > 1 { (gaps.iter().map(|g| (g - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt() } else { 0.0 };
        let t = if sd > 0.0 { mean / (sd / (n as f64).sqrt()) } else if mean != 0.0 && n > 1 { f64::INFINITY.copysign(mean) } else { 0.0 };
        let (mut real_usd, mut shadow_usd) = (0.0, 0.0);
        let (mut same, mut other_outcome, mut missed, mut extra) = (0, 0, 0, 0);
        for c in items {
            match *c {
                Compared::Both { same: s, real, shadow } => {
                    real_usd += real;
                    shadow_usd += shadow;
                    if s {
                        same += 1
                    } else {
                        other_outcome += 1
                    }
                }
                Compared::Missed { shadow } => {
                    shadow_usd += shadow;
                    missed += 1
                }
                Compared::Extra { real } => {
                    real_usd += real;
                    extra += 1
                }
            }
        }
        Tally { n, same, other_outcome, missed, extra, real_usd, shadow_usd, t }
    }

    pub fn gap(&self) -> f64 {
        self.real_usd - self.shadow_usd
    }

    pub fn gap_per_trade(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.gap() / self.n as f64 }
    }

    /// The divergence rule in the module doc.
    pub fn diverged(&self, items_agree: usize) -> bool {
        if self.n < MIN_PAIRS {
            return false;
        }
        let consistent_gap = self.gap_per_trade().abs() >= GAP_USD && self.t.abs() >= T_STAT;
        let disagree = (self.n - items_agree) as f64 / self.n as f64 > MAX_DISAGREE;
        consistent_gap || disagree
    }
}

fn line(label: &str, t: &Tally) -> String {
    let mut parts = vec![format!("{} same", t.same)];
    if t.other_outcome > 0 {
        parts.push(format!("{} other outcome", t.other_outcome));
    }
    if t.missed > 0 {
        parts.push(format!("{} missed fill", t.missed));
    }
    if t.extra > 0 {
        parts.push(format!("{} extra fill", t.extra));
    }
    format!(
        "{label}: {} compared — {} · real ${:+.2} vs shadow ${:+.2} (gap ${:+.2}, ${:+.2}/trade)",
        t.n,
        parts.join(", "),
        t.real_usd,
        t.shadow_usd,
        t.gap(),
        t.gap_per_trade()
    )
}

/// The summary section for every strategy/mode with an executed order, or
/// `None` when nothing was ever executed.
pub fn section(pairs: &[Pair], date: NaiveDate) -> Option<String> {
    let mut keys: Vec<(String, String)> = pairs.iter().map(|p| (p.strategy.clone(), p.mode.clone())).collect();
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return None;
    }
    let mut out = String::from("\n<b>Shadow vs paper/live</b>\n");
    for (strategy, mode) in keys {
        let mine: Vec<&Pair> = pairs.iter().filter(|p| p.strategy == strategy && p.mode == mode).collect();
        let all: Vec<Compared> = mine.iter().filter_map(|p| compare(p)).collect();
        let today: Vec<Compared> = mine.iter().filter(|p| p.trading_date == date).filter_map(|p| compare(p)).collect();
        let total = Tally::of(&all);
        let agree = all.iter().filter(|c| c.agrees()).count();
        let verdict = if total.diverged(agree) {
            "⚠️ DIVERGED".to_string()
        } else if total.n < MIN_PAIRS {
            format!("ok so far (judged from {MIN_PAIRS} trades)")
        } else {
            "ok".to_string()
        };
        out.push_str(&format!("<b>{strategy}</b> · {mode}\n"));
        if !today.is_empty() {
            out.push_str(&line("Today", &Tally::of(&today)));
            out.push('\n');
        }
        out.push_str(&line("Total", &total));
        out.push_str(&format!(" · {verdict}\n"));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, n).unwrap()
    }

    fn pair(strategy: &str, d: u32, status: &str, exit: Option<&str>, real: Option<f64>, shadow: &str, shadow_usd: Option<f64>) -> Pair {
        Pair {
            mode: "paper".into(),
            strategy: strategy.into(),
            trading_date: day(d),
            status: status.into(),
            exit_reason: exit.map(str::to_string),
            realized_usd: real,
            shadow_outcome: shadow.into(),
            shadow_usd,
        }
    }

    #[test]
    fn each_order_is_compared_with_its_shadow_trade() {
        let s = "fade-priorpoc-2r";
        assert_eq!(compare(&pair(s, 21, "closed", Some("target"), Some(80.0), "won", Some(75.0))), Some(Compared::Both { same: true, real: 80.0, shadow: 75.0 }));
        assert_eq!(compare(&pair(s, 21, "closed", Some("stop"), Some(-50.0), "lost", Some(-50.0))), Some(Compared::Both { same: true, real: -50.0, shadow: -50.0 }));
        assert_eq!(compare(&pair(s, 21, "closed", Some("close"), Some(5.0), "expired", Some(2.5))), Some(Compared::Both { same: true, real: 5.0, shadow: 2.5 }));
        assert_eq!(compare(&pair(s, 21, "closed", Some("stop"), Some(-50.0), "won", Some(75.0))), Some(Compared::Both { same: false, real: -50.0, shadow: 75.0 }));
        assert_eq!(compare(&pair(s, 21, "cancelled", Some("cancel"), None, "won", Some(75.0))), Some(Compared::Missed { shadow: 75.0 }), "shadow traded, the order never filled");
        assert_eq!(compare(&pair(s, 21, "closed", Some("stop"), Some(-50.0), "invalidated", None)), Some(Compared::Extra { real: -50.0 }));
        assert_eq!(compare(&pair(s, 21, "cancelled", Some("cancel"), None, "invalidated", None)), None, "neither traded");
        assert_eq!(compare(&pair(s, 21, "filled", None, None, "entered", None)), None, "still open");
        assert_eq!(compare(&pair(s, 21, "closed", Some("stop"), Some(-50.0), "entered", None)), None, "shadow still open");
    }

    #[test]
    fn small_or_few_gaps_are_not_a_divergence() {
        // the 2026-10-21..23 paper record: 7 trades, same outcomes, +$23.75
        let gaps = [(-50.0, -50.0), (80.0, 75.0), (-75.0, -75.0), (-48.75, -50.0), (75.0, 75.0), (77.5, 75.0), (-35.0, -50.0)];
        let items: Vec<Compared> = gaps.iter().map(|&(r, s)| Compared::Both { same: true, real: r, shadow: s }).collect();
        let t = Tally::of(&items);
        assert_eq!((t.n, t.same), (7, 7));
        assert!((t.gap() - 23.75).abs() < 1e-9);
        assert!(!t.diverged(7), "$3.39/trade but t = {:.2}", t.t);
        let few = [Compared::Both { same: false, real: -50.0, shadow: 75.0 }, Compared::Missed { shadow: 75.0 }];
        assert!(!Tally::of(&few).diverged(0), "under 5 trades nothing is called");
    }

    #[test]
    fn a_consistent_gap_or_too_many_disagreements_is_a_divergence() {
        let slip: Vec<Compared> = [-11.0, -13.0, -12.0, -12.5, -11.5, -12.0].iter().map(|&g| Compared::Both { same: true, real: 60.0 + g, shadow: 60.0 }).collect();
        let t = Tally::of(&slip);
        assert!(t.diverged(6), "-$12/trade every time: gap {}, t {}", t.gap_per_trade(), t.t);
        let mut misses = vec![Compared::Both { same: true, real: 60.0, shadow: 60.0 }; 6];
        misses.extend([Compared::Missed { shadow: 60.0 }, Compared::Missed { shadow: -60.0 }]);
        let t = Tally::of(&misses);
        assert!(t.diverged(6), "2 of 8 disagree (25%)");
        assert_eq!(t.missed, 2);
    }

    #[test]
    fn the_section_shows_today_the_total_and_the_verdict() {
        let s = "fade-priorpoc-va80-1.5r";
        let pairs = vec![
            pair(s, 21, "closed", Some("target"), Some(80.0), "won", Some(75.0)),
            pair(s, 23, "closed", Some("stop"), Some(-35.0), "lost", Some(-50.0)),
            pair(s, 23, "cancelled", Some("cancel"), None, "won", Some(75.0)),
            Pair { mode: "live".into(), ..pair("fade-roll-overnight", 23, "closed", Some("target"), Some(58.75), "won", Some(60.0)) },
        ];
        let text = section(&pairs, day(23)).expect("section");
        assert!(text.contains("<b>fade-priorpoc-va80-1.5r</b> · paper"), "{text}");
        assert!(text.contains("Today: 2 compared — 1 same, 1 missed fill"), "{text}");
        assert!(text.contains("Total: 3 compared — 2 same, 1 missed fill · real $+45.00 vs shadow $+100.00 (gap $-55.00"), "{text}");
        assert!(text.contains("ok so far (judged from 5 trades)"), "{text}");
        assert!(text.contains("<b>fade-roll-overnight</b> · live"), "{text}");
        assert_eq!(section(&[], day(23)), None);
    }
}
