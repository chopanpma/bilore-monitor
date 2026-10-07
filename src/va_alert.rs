//! Value-area alert for manual trading (MON-011, user request 2026-10-07) —
//! pure; `main.rs` feeds trades and sends the Telegram message.
//!
//! When price leaves the value area of the CURRENT session's developing
//! profile (RTH from 08:30 CT, Globex from 17:00 CT), the profile is a
//! normal shape (D, P or b — never irregular), and at least 2 of
//! structure, order flow and the other timeframe agree with the fade back
//! toward the POC (SHORT above VAH, LONG below VAL), send one alert. At most
//! one per profile period (30 min RTH, 60 min Globex) unless price has
//! crossed the POC since the last one.
//!
//! Shape thresholds are first-pass values (MON-011): classify only after 2
//! completed periods; double distribution = on the profile smoothed over 5
//! points, a second local peak of at least 40% of the main one, at least
//! 20% of the range away, with a valley of at most half that peak between
//! them; elongated = value area wider than 65% of the range; otherwise
//! D/P/b by the POC's third of the range.

use bilore_core::confirmation::{order_flow_check, SignalCheck};
use bilore_core::market_structure::Direction;
use bilore_ml_rs::poc_anchor::VolumeProfile;
use chrono::{Datelike, Duration, NaiveDateTime, NaiveTime, Weekday};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Completed periods before a profile's shape counts.
const MIN_PERIODS: u32 = 2;
/// Double distribution, on the profile smoothed over `SMOOTH_POINTS` 1-point
/// buckets: a second local peak at least `DD_PEAK` of the main one, at least
/// `DD_APART` of the range away from it, with a valley of at most
/// `DD_VALLEY` of that second peak between them. Raw 1-point buckets are
/// too jagged: a single thin price called a normal day irregular
/// (2026-10-06, measured on 16 RTH sessions when this was written).
const DD_PEAK: f64 = 0.40;
const DD_APART: f64 = 0.20;
const DD_VALLEY: f64 = 0.50;
const SMOOTH_POINTS: usize = 5;
/// Elongated: value area wider than this share of the range.
const ELONGATED_VA: f64 = 0.65;
/// Bucket for the double-distribution check, in ticks (1 point).
const SHAPE_BUCKET_TICKS: u32 = 4;
/// Width of the longest volume bar in the text profile, in characters.
const BAR_WIDTH: usize = 16;

fn hm(h: u32, m: u32) -> NaiveTime {
    NaiveTime::from_hms_opt(h, m, 0).unwrap()
}

#[cfg(test)]
use chrono::NaiveDate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Rth,
    Globex,
}

/// One trading session: its kind and Chicago wall-clock start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId {
    pub kind: SessionKind,
    pub start: NaiveDateTime,
}

/// RTH = Mon-Fri 08:30-15:00 CT; Globex = 17:00 CT (Sun-Thu) to 08:30 CT
/// next morning. 15:00-17:00, Friday evening and Saturday: `None`.
pub fn session_of(at: NaiveDateTime) -> Option<SessionId> {
    use Weekday::*;
    let (day, t) = (at.weekday(), at.time());
    let weekday = !matches!(day, Sat | Sun);
    if weekday && t >= hm(8, 30) && t < hm(15, 0) {
        return Some(SessionId { kind: SessionKind::Rth, start: at.date().and_time(hm(8, 30)) });
    }
    if t >= hm(17, 0) && matches!(day, Sun | Mon | Tue | Wed | Thu) {
        return Some(SessionId { kind: SessionKind::Globex, start: at.date().and_time(hm(17, 0)) });
    }
    if weekday && t < hm(8, 30) {
        // Monday early belongs to Sunday 17:00
        let opened = at.date() - Duration::days(1);
        return Some(SessionId { kind: SessionKind::Globex, start: opened.and_time(hm(17, 0)) });
    }
    None
}

/// Index of the profile period `at` falls in: 30-minute periods in RTH
/// (A = 0), 60-minute periods in Globex. Also the number of completed periods.
pub fn period_of(session: &SessionId, at: NaiveDateTime) -> u32 {
    let minutes = (at - session.start).num_minutes().max(0) as u32;
    match session.kind {
        SessionKind::Rth => minutes / 30,
        SessionKind::Globex => minutes / 60,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Bell: POC in the middle third of the range.
    D,
    /// POC in the upper third.
    P,
    /// POC in the lower third (`b`).
    B,
    /// Double distribution or elongated (no bulge): never alerts.
    Irregular,
}

/// Shape of a profile with `tick` price increments; `None` when empty.
pub fn classify_shape(profile: &VolumeProfile, tick: Decimal) -> Option<Shape> {
    let levels: Vec<(Decimal, u64)> = profile.levels().collect();
    let (low, high) = (levels.first()?.0, levels.last()?.0);
    let poc = profile.poc()?;
    let (val, vah) = profile.value_area()?;
    let range = high - low;
    if range <= Decimal::ZERO {
        return Some(Shape::D);
    }
    let share = |x: Decimal| (x / range).to_f64().unwrap_or(0.0);
    if share(vah - val) > ELONGATED_VA || double_distribution(&levels, low, tick) {
        return Some(Shape::Irregular);
    }
    let at = share(poc - low);
    Some(if at > 2.0 / 3.0 {
        Shape::P
    } else if at < 1.0 / 3.0 {
        Shape::B
    } else {
        Shape::D
    })
}

/// Volume per 1-point bucket from `low` (empty buckets included), smoothed
/// with a centered `SMOOTH_POINTS` moving average; true when another local
/// peak qualifies as a second distribution (see `DD_PEAK`).
fn double_distribution(levels: &[(Decimal, u64)], low: Decimal, tick: Decimal) -> bool {
    let bucket = tick * Decimal::from(SHAPE_BUCKET_TICKS);
    let idx = |p: Decimal| ((p - low) / bucket).floor().to_usize().unwrap_or(0);
    let mut raw = vec![0f64; idx(levels.last().map_or(low, |l| l.0)) + 1];
    for &(p, v) in levels {
        raw[idx(p)] += v as f64;
    }
    let half = SMOOTH_POINTS / 2;
    let sm: Vec<f64> = (0..raw.len())
        .map(|i| raw[i.saturating_sub(half)..(i + half + 1).min(raw.len())].iter().sum::<f64>() / SMOOTH_POINTS as f64)
        .collect();
    let Some((main, &peak)) = sm.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else { return false };
    let range_points = (raw.len().max(2) - 1) as f64;
    let last = sm.len() - 1;
    (0..sm.len()).any(|j| {
        let (a, b) = (main.min(j), main.max(j));
        let local_peak = (j == 0 || sm[j] >= sm[j - 1]) && (j == last || sm[j] >= sm[j + 1]);
        b - a >= 2
            && local_peak
            && sm[j] >= DD_PEAK * peak
            && (b - a) as f64 >= DD_APART * range_points
            && sm[a + 1..b].iter().copied().fold(f64::INFINITY, f64::min) <= DD_VALLEY * sm[j]
    })
}

/// The current session's developing profile; restarts at each session start.
#[derive(Debug, Clone, Default)]
pub struct DevelopingProfile {
    pub session: Option<SessionId>,
    pub volume: VolumeProfile,
}

impl DevelopingProfile {
    /// Add a trade; trades outside any session are ignored.
    pub fn on_trade(&mut self, at: NaiveDateTime, price: Decimal, size: u64) {
        let Some(session) = session_of(at) else { return };
        if self.session != Some(session) {
            *self = Self { session: Some(session), ..Self::default() };
        }
        self.volume.add(price, size);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Above VAH: fade SHORT.
    Upper,
    /// Below VAL: fade LONG.
    Lower,
}

/// One factor's read against the fade direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    Agree,
    Disagree,
    /// No direction, or the input is unavailable — never counts.
    Neutral,
}

/// The three inputs, as directions (or none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Factors {
    /// `market_structure::structure_direction` of the session's 5-min bars.
    pub structure: Option<Direction>,
    /// Session cumulative delta (tick rule); directional at |delta| >= 500.
    pub delta: i64,
    /// Other timeframe: `inventory::participant_view(..).lean` for the
    /// session type's prior 1/5/20-session composites; `None` = neutral or
    /// unavailable.
    pub otf: Option<Direction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub session: SessionId,
    pub period: u32,
    pub side: Side,
    pub fade: Direction,
    pub shape: Shape,
    pub structure: Read,
    pub order_flow: Read,
    pub otf: Read,
    pub delta: i64,
    pub price: Decimal,
    pub poc: Decimal,
    pub vah: Decimal,
    pub val: Decimal,
}

impl Alert {
    /// How many of structure, order flow and other timeframe agree.
    pub fn agreeing(&self) -> usize {
        [self.structure, self.order_flow, self.otf].iter().filter(|r| **r == Read::Agree).count()
    }
}

/// The alert for `price` at `at`, if every MON-011 condition holds
/// (throttling is `AlertThrottle`'s job).
pub fn evaluate(profile: &DevelopingProfile, at: NaiveDateTime, price: Decimal, factors: &Factors, tick: Decimal) -> Option<Alert> {
    let session = profile.session.filter(|s| session_of(at) == Some(*s))?;
    let period = period_of(&session, at);
    if period < MIN_PERIODS {
        return None;
    }
    let shape = classify_shape(&profile.volume, tick).filter(|s| *s != Shape::Irregular)?;
    let poc = profile.volume.poc()?;
    let (val, vah) = profile.volume.value_area()?;
    let (side, fade) = if price > vah {
        (Side::Upper, Direction::Short)
    } else if price < val {
        (Side::Lower, Direction::Long)
    } else {
        return None;
    };
    let read = |dir: Option<Direction>| match dir {
        Some(d) if d == fade => Read::Agree,
        Some(_) => Read::Disagree,
        None => Read::Neutral,
    };
    let order_flow = match order_flow_check(factors.delta, fade) {
        SignalCheck::Confirm => Read::Agree,
        SignalCheck::Conflict => Read::Disagree,
        SignalCheck::Neutral => Read::Neutral,
    };
    let alert = Alert {
        session,
        period,
        side,
        fade,
        shape,
        structure: read(factors.structure),
        order_flow,
        otf: read(factors.otf),
        delta: factors.delta,
        price,
        poc,
        vah,
        val,
    };
    (alert.agreeing() >= 2).then_some(alert)
}

/// One alert per profile period unless price crossed the POC since.
#[derive(Debug, Clone, Default)]
pub struct AlertThrottle {
    /// Session, period and side of the last alert sent.
    last: Option<(SessionId, u32, Side)>,
    /// Price has crossed the POC since the last alert.
    crossed_poc: bool,
}

impl AlertThrottle {
    /// Call on every live trade with the current POC.
    pub fn observe(&mut self, price: Decimal, poc: Decimal) {
        let crossed = match self.last {
            Some((_, _, Side::Upper)) => price <= poc,
            Some((_, _, Side::Lower)) => price >= poc,
            None => false,
        };
        self.crossed_poc |= crossed;
    }

    /// `true` (and records it) when `alert` may be sent now.
    pub fn should_send(&mut self, alert: &Alert) -> bool {
        let same_period = self.last.is_some_and(|(s, p, _)| s == alert.session && p == alert.period);
        if same_period && !self.crossed_poc {
            return false;
        }
        self.last = Some((alert.session, alert.period, alert.side));
        self.crossed_poc = false;
        true
    }
}

/// Text profile, highest price first, at most `max_rows` rows: each row a
/// price, a volume bar and VAH/POC/VAL markers, with `◀` on the price's row.
pub fn render_profile(
    profile: &VolumeProfile,
    vah: Decimal,
    poc: Decimal,
    val: Decimal,
    price: Decimal,
    tick: Decimal,
    max_rows: usize,
) -> String {
    let levels: Vec<(Decimal, u64)> = profile.levels().collect();
    let (Some(first), Some(last)) = (levels.first(), levels.last()) else { return String::new() };
    let (lo, high) = (first.0.min(price), last.0.max(price));
    // Round row sizes (0.25, 0.5, 1, 2, 2.5, 5, 10... points at a 0.25 tick)
    // aligned to round prices, so row labels read like chart levels.
    let rows_for = |bucket: Decimal| {
        let low = (lo / bucket).floor() * bucket;
        (((high - low) / bucket).floor().to_usize().unwrap_or(0) + 1, low)
    };
    let mut bucket = tick;
    for k in [1u64, 2, 4, 8, 10, 20, 40, 80, 100, 200, 400] {
        bucket = tick * Decimal::from(k);
        if rows_for(bucket).0 <= max_rows.max(1) {
            break;
        }
    }
    let (rows, low) = rows_for(bucket);
    let row_of = |p: Decimal| ((p - low) / bucket).floor().to_usize().unwrap_or(0).min(rows - 1);
    let mut vol = vec![0u64; rows];
    for &(p, v) in &levels {
        vol[row_of(p)] += v;
    }
    let max_vol = vol.iter().copied().max().unwrap_or(0).max(1);
    let mut out = Vec::with_capacity(rows);
    for i in (0..rows).rev() {
        let bar_len = if vol[i] == 0 { 0 } else { ((vol[i] * BAR_WIDTH as u64 + max_vol / 2) / max_vol).max(1) as usize };
        let mut marks = Vec::new();
        for (level, name) in [(vah, "VAH"), (poc, "POC"), (val, "VAL")] {
            if row_of(level) == i {
                marks.push(name.to_string());
            }
        }
        if row_of(price) == i {
            marks.push(format!("◀ {price:.2}"));
        }
        let line = format!("{:.2} {:<width$} {}", low + bucket * Decimal::from(i as u64), "█".repeat(bar_len), marks.join(" "), width = BAR_WIDTH);
        out.push(line.trim_end().to_string());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    /// Chicago wall clock: 2026-10-02 Fri, 10-03 Sat, 10-04 Sun, 10-05 Mon, 10-06 Tue.
    fn ct(day: u32, h: u32, m: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    fn tick() -> Decimal {
        d("0.25")
    }

    /// Volumes at 5000.00, 5001.00, ... (one per whole point).
    fn volume(vols: &[u64]) -> VolumeProfile {
        let mut v = VolumeProfile::default();
        for (i, &vol) in vols.iter().enumerate() {
            v.add(d("5000.00") + Decimal::from(i as u32), vol);
        }
        v
    }

    fn rth_profile(vols: &[u64]) -> DevelopingProfile {
        let mut p = DevelopingProfile::default();
        for (i, &vol) in vols.iter().enumerate() {
            p.on_trade(ct(5, 8, 35), d("5000.00") + Decimal::from(i as u32), vol);
        }
        p
    }

    const BELL: [u64; 11] = [5, 10, 20, 40, 70, 100, 70, 40, 20, 10, 5]; // POC 5005, VA 5004-5007
    const UPPER_HEAVY: [u64; 11] = [5, 5, 5, 10, 20, 40, 70, 100, 120, 90, 40]; // POC 5008
    const LOWER_HEAVY: [u64; 11] = [40, 90, 120, 100, 70, 40, 20, 10, 5, 5, 5]; // POC 5002
    /// A tall bell at 5002 and a separate, lower hump at 5008-5012 (value
    /// area 50% of the range, so only the double-distribution rule catches it).
    const DOUBLE: [u64; 13] = [10, 60, 450, 60, 10, 2, 2, 2, 50, 50, 50, 50, 50];
    const FLAT: [u64; 11] = [50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 55];

    fn factors(structure: Option<Direction>, delta: i64, otf: Option<Direction>) -> Factors {
        Factors { structure, delta, otf }
    }

    // ── sessions and the developing profile ─────────────────────────────────

    #[test]
    fn session_of_maps_rth_globex_and_the_gap() {
        let rth = Some(SessionId { kind: SessionKind::Rth, start: ct(5, 8, 30) });
        let mon_globex = Some(SessionId { kind: SessionKind::Globex, start: ct(5, 17, 0) });
        assert_eq!(session_of(ct(5, 8, 30)), rth);
        assert_eq!(session_of(ct(5, 14, 59)), rth);
        assert_eq!(session_of(ct(5, 15, 0)), None);
        assert_eq!(session_of(ct(5, 16, 59)), None);
        assert_eq!(session_of(ct(5, 17, 0)), mon_globex);
        assert_eq!(session_of(ct(6, 2, 0)), mon_globex, "after midnight, same Globex session");
        assert_eq!(session_of(ct(5, 2, 0)), Some(SessionId { kind: SessionKind::Globex, start: ct(4, 17, 0) }), "Monday early = Sunday's Globex");
        assert_eq!(session_of(ct(2, 17, 0)), None, "Friday evening");
        assert_eq!(session_of(ct(3, 12, 0)), None, "Saturday");
    }

    #[test]
    fn periods_are_30_minutes_in_rth_and_60_in_globex() {
        let rth = session_of(ct(5, 9, 0)).unwrap();
        assert_eq!(period_of(&rth, ct(5, 8, 30)), 0);
        assert_eq!(period_of(&rth, ct(5, 9, 29)), 1);
        assert_eq!(period_of(&rth, ct(5, 9, 30)), 2);
        let globex = session_of(ct(5, 20, 0)).unwrap();
        assert_eq!(period_of(&globex, ct(5, 17, 59)), 0);
        assert_eq!(period_of(&globex, ct(6, 2, 0)), 9);
    }

    #[test]
    fn the_profile_restarts_at_each_session_start() {
        let mut p = rth_profile(&BELL);
        p.on_trade(ct(5, 15, 30), d("5100.00"), 7); // between sessions: ignored
        p.on_trade(ct(5, 17, 0), d("5020.00"), 3);
        assert_eq!(p.session, session_of(ct(5, 17, 0)));
        assert_eq!(p.volume.levels().collect::<Vec<_>>(), vec![(d("5020.00"), 3)]);
    }

    // ── shape ────────────────────────────────────────────────────────────────

    #[test]
    fn a_bell_shaped_profile_is_d() {
        assert_eq!(classify_shape(&volume(&BELL), tick()), Some(Shape::D));
    }

    #[test]
    fn a_profile_with_its_poc_in_the_upper_third_is_p() {
        assert_eq!(classify_shape(&volume(&UPPER_HEAVY), tick()), Some(Shape::P));
    }

    #[test]
    fn a_profile_with_its_poc_in_the_lower_third_is_b() {
        assert_eq!(classify_shape(&volume(&LOWER_HEAVY), tick()), Some(Shape::B));
    }

    #[test]
    fn a_double_distribution_is_irregular() {
        assert_eq!(classify_shape(&volume(&DOUBLE), tick()), Some(Shape::Irregular));
    }

    #[test]
    fn a_thin_elongated_profile_is_irregular() {
        assert_eq!(classify_shape(&volume(&FLAT), tick()), Some(Shape::Irregular));
    }

    #[test]
    fn too_early_in_the_session_is_not_classified() {
        let p = rth_profile(&BELL);
        let all = factors(Some(Direction::Short), -800, Some(Direction::Short));
        assert_eq!(evaluate(&p, ct(5, 9, 29), d("5009.00"), &all, tick()), None, "only period A completed");
        assert!(evaluate(&p, ct(5, 9, 30), d("5009.00"), &all, tick()).is_some(), "A and B completed");
    }

    // ── trigger, side and alignment ──────────────────────────────────────────

    #[test]
    fn price_above_vah_fades_short() {
        let a = evaluate(&rth_profile(&BELL), ct(5, 10, 0), d("5009.00"), &factors(Some(Direction::Short), -800, Some(Direction::Long)), tick())
            .expect("alert");
        assert_eq!((a.side, a.fade, a.shape), (Side::Upper, Direction::Short, Shape::D));
        assert_eq!((a.poc, a.vah, a.val), (d("5005.00"), d("5007.00"), d("5004.00")));
        assert_eq!(a.period, 3);
    }

    #[test]
    fn price_below_val_fades_long() {
        let a = evaluate(&rth_profile(&BELL), ct(5, 10, 0), d("5002.00"), &factors(Some(Direction::Long), 800, Some(Direction::Short)), tick())
            .expect("alert");
        assert_eq!((a.side, a.fade), (Side::Lower, Direction::Long));
    }

    #[test]
    fn price_inside_the_value_area_never_alerts() {
        let all = factors(Some(Direction::Short), -800, Some(Direction::Short));
        assert_eq!(evaluate(&rth_profile(&BELL), ct(5, 10, 0), d("5005.00"), &all, tick()), None);
    }

    #[test]
    fn an_irregular_profile_never_alerts() {
        let all = factors(Some(Direction::Short), -800, Some(Direction::Short));
        assert_eq!(evaluate(&rth_profile(&DOUBLE), ct(5, 10, 0), d("5012.00"), &all, tick()), None);
    }

    #[test]
    fn two_of_three_agreeing_with_the_fade_alerts() {
        let a = evaluate(&rth_profile(&BELL), ct(5, 10, 0), d("5009.00"), &factors(Some(Direction::Short), -800, Some(Direction::Long)), tick())
            .expect("alert");
        assert_eq!((a.structure, a.order_flow, a.otf), (Read::Agree, Read::Agree, Read::Disagree));
        assert_eq!(a.agreeing(), 2);
    }

    #[test]
    fn one_of_three_does_not_alert() {
        let f = factors(Some(Direction::Short), 800, Some(Direction::Long));
        assert_eq!(evaluate(&rth_profile(&BELL), ct(5, 10, 0), d("5009.00"), &f, tick()), None);
    }

    #[test]
    fn neutral_factors_do_not_count() {
        let f = factors(Some(Direction::Short), -300, None);
        assert_eq!(evaluate(&rth_profile(&BELL), ct(5, 10, 0), d("5009.00"), &f, tick()), None);
    }

    // ── throttle ─────────────────────────────────────────────────────────────

    fn alert_at(at: NaiveDateTime) -> Alert {
        evaluate(&rth_profile(&BELL), at, d("5009.00"), &factors(Some(Direction::Short), -800, None), tick()).expect("alert")
    }

    #[test]
    fn one_alert_per_period() {
        let mut t = AlertThrottle::default();
        assert!(t.should_send(&alert_at(ct(5, 9, 35))), "period C");
        t.observe(d("5008.00"), d("5005.00"));
        assert!(!t.should_send(&alert_at(ct(5, 9, 50))), "still period C, no POC cross");
    }

    #[test]
    fn a_poc_cross_re_arms_within_the_period() {
        let mut t = AlertThrottle::default();
        assert!(t.should_send(&alert_at(ct(5, 9, 35))));
        t.observe(d("5005.00"), d("5005.00")); // traded down to the POC
        t.observe(d("5009.00"), d("5005.00"));
        assert!(t.should_send(&alert_at(ct(5, 9, 50))));
    }

    #[test]
    fn a_new_period_re_arms() {
        let mut t = AlertThrottle::default();
        assert!(t.should_send(&alert_at(ct(5, 9, 35))), "period C");
        assert!(t.should_send(&alert_at(ct(5, 10, 5))), "period D");
    }

    // ── text profile ─────────────────────────────────────────────────────────

    #[test]
    fn the_text_profile_marks_vah_poc_val_and_price() {
        let text = render_profile(&volume(&BELL), d("5007.00"), d("5005.00"), d("5004.00"), d("5009.00"), tick(), 24);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("5010.00"), "{text}");
        let row = |p: &str| lines.iter().find(|l| l.starts_with(p)).unwrap_or_else(|| panic!("no row {p}\n{text}"));
        assert!(row("5007.00").contains("VAH"), "{text}");
        assert!(row("5005.00").contains("POC"), "{text}");
        assert!(row("5004.00").contains("VAL"), "{text}");
        assert!(row("5009.00").contains('◀'), "{text}");
        assert!(row("5005.00").contains('█'), "{text}");
    }

    #[test]
    fn the_text_profile_fits_in_24_rows() {
        let mut v = VolumeProfile::default();
        let mut p = d("5000.00");
        while p <= d("5060.00") {
            v.add(p, 1);
            p += tick();
        }
        let text = render_profile(&v, d("5040.00"), d("5030.00"), d("5020.00"), d("5050.00"), tick(), 24);
        assert!(text.lines().count() <= 24, "{} rows\n{text}", text.lines().count());
    }
}
