//! Value-area alert study (MON-011, user request 2026-10-26): replay the
//! alert on recorded ticks exactly as `main.rs` runs it, and write one CSV
//! row per setup with the cockpit's other readings at that moment and what
//! price did afterwards. The CSV is analysed separately; nothing here
//! decides or trades.
//!
//! Rows: every alert the live code would send (`sent = 1`, `evaluate` and the
//! same throttle; MON-022 since 2026-10-26, `agreeing` keeps MON-011's count), plus the first candidate per period (price outside a normally
//! shaped value area, any number of factors agreeing; own throttle), so the
//! 2-of-3 rule itself can be judged.
//!
//! Sessions: RTH days that pass `poc_anchor::fully_recorded`, Globex sessions
//! with ticks in at least `GLOBEX_MIN_MINUTES` of their 930 minutes; each
//! session uses the contract with the most ticks. Other timeframe: prior
//! 1/5/20 sessions of the same contract and type, rebuilt from ticks with
//! tpo-builder's session configs (live reads the same thing from tpo_bars).
//!
//! Usage: `cargo run --release --bin backtest_va_alert -- [--from 2026-03-13] [--out va_alert_study.csv]`

use anyhow::Result;
use bilore_core::market_structure::{analyze_structure, structure_direction, Bar, Direction};
use bilore_core::order_flow::{Side as FlowSide, Tick, TickRuleClassifier};
use bilore_core::inventory::{participant_view, ReferenceProfile};
use bilore_monitor::va_alert::{self, AlertThrottle, DevelopingProfile, Factors, Read, SessionKind};
use bilore_ml_rs::poc_anchor::fully_recorded;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::America::Chicago;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use tpo_builder::profile::{compute_metrics, overnight_session_config, rth_session_config, TpoEntry, TpoProfile};

const GLOBEX_MIN_MINUTES: i64 = 860;
const LARGE_PRINT_MIN: i64 = 50; // cockpit `large_print_min`
const MAX_K: usize = 20; // first-passage points recorded, 1..=MAX_K

/// Best contract per (kind, session date): symbol, first and last tick time, minutes with ticks, ticks.
type Coverage = BTreeMap<(String, NaiveDate), (String, NaiveTime, NaiveTime, i64, i64)>;

fn f(x: Decimal) -> f64 {
    x.to_f64().unwrap_or(f64::NAN)
}

fn ct_to_utc(t: NaiveDateTime) -> DateTime<Utc> {
    t.and_local_timezone(Chicago).earliest().unwrap().with_timezone(&Utc)
}

/// What price did after an entry at `entry` in direction `fade`, over the
/// ticks to the session end: seconds until the first favourable and
/// adverse move of k points (k = 1..=MAX_K), seconds until the POC traded,
/// the worst adverse move before it, the excursions at 15/30/60 minutes and
/// the move to the session's last price, all signed so + = in the fade's favour.
#[derive(Debug, Default, Clone, PartialEq)]
struct Outcome {
    fav: Vec<Option<f64>>,
    adv: Vec<Option<f64>>,
    poc_s: Option<f64>,
    mae_before_poc: f64,
    mfe: [f64; 3],
    mae: [f64; 3],
    close_move: f64,
}

fn outcome(after: &[(DateTime<Utc>, Decimal)], t0: DateTime<Utc>, entry: Decimal, fade: Direction, poc: Decimal) -> Outcome {
    let sign = if fade == Direction::Long { 1.0 } else { -1.0 };
    let mut o = Outcome { fav: vec![None; MAX_K], adv: vec![None; MAX_K], ..Default::default() };
    let (mut best, mut worst) = (0f64, 0f64);
    let horizons = [15.0 * 60.0, 30.0 * 60.0, 60.0 * 60.0];
    for &(ts, p) in after {
        let s = (ts - t0).num_milliseconds() as f64 / 1000.0;
        let m = sign * f(p - entry);
        best = best.max(m);
        worst = worst.min(m);
        for (i, h) in horizons.iter().enumerate() {
            if s <= *h {
                o.mfe[i] = o.mfe[i].max(m);
                o.mae[i] = o.mae[i].min(m);
            }
        }
        for k in 1..=MAX_K {
            if o.fav[k - 1].is_none() && best >= k as f64 {
                o.fav[k - 1] = Some(s);
            }
            if o.adv[k - 1].is_none() && worst <= -(k as f64) {
                o.adv[k - 1] = Some(s);
            }
        }
        if o.poc_s.is_none() {
            let reached = if fade == Direction::Long { p >= poc } else { p <= poc };
            if reached {
                o.poc_s = Some(s);
                o.mae_before_poc = worst;
            }
        }
        o.close_move = m;
    }
    if o.poc_s.is_none() {
        o.mae_before_poc = worst;
    }
    o
}

/// Rolling last-hour window of classified ticks for the short-term readings.
#[derive(Default)]
struct Window {
    ticks: VecDeque<(DateTime<Utc>, Decimal, i64, FlowSide)>,
}

impl Window {
    fn push(&mut self, ts: DateTime<Utc>, p: Decimal, size: i64, side: FlowSide) {
        self.ticks.push_back((ts, p, size, side));
        while self.ticks.front().is_some_and(|t| ts - t.0 > Duration::minutes(60)) {
            self.ticks.pop_front();
        }
    }
    /// (range, move, buy-sell imbalance in [-1, 1], delta) over the last `mins`.
    fn stats(&self, now: DateTime<Utc>, price: Decimal, mins: i64) -> (f64, f64, f64, i64) {
        let from = now - Duration::minutes(mins);
        let w = self.ticks.iter().filter(|t| t.0 >= from);
        let (mut hi, mut lo, mut first, mut buy, mut sell) = (price, price, None, 0i64, 0i64);
        for t in w {
            hi = hi.max(t.1);
            lo = lo.min(t.1);
            first.get_or_insert(t.1);
            match t.3 {
                FlowSide::Buy => buy += t.2,
                FlowSide::Sell => sell += t.2,
            }
        }
        let tot = (buy + sell).max(1) as f64;
        (f(hi - lo), f(price - first.unwrap_or(price)), (buy - sell) as f64 / tot, buy - sell)
    }
}

struct Session {
    kind: SessionKind,
    date: NaiveDate,
    symbol: String,
    start: NaiveDateTime,
    end: NaiveDateTime,
}

/// TPO entries per (contract, kind) by session date, for the other timeframe.
type Tpo = BTreeMap<NaiveDate, Vec<TpoEntry>>;

async fn build_tpo(pool: &PgPool, symbol: &str) -> Result<(Tpo, Tpo)> {
    let rows: Vec<(DateTime<Utc>, Decimal, i32)> =
        sqlx::query_as("SELECT ts, price, size FROM tick_trades WHERE symbol = $1 ORDER BY ts").bind(symbol).fetch_all(pool).await?;
    let mut rth = TpoProfile::new(symbol, rth_session_config());
    let mut glx = TpoProfile::new(format!("{symbol}-GLOBEX"), overnight_session_config());
    for (ts, p, s) in &rows {
        rth.add_trade(*p, (*s).max(0) as u64, *ts);
        glx.add_trade(*p, (*s).max(0) as u64, *ts);
    }
    let by_date = |prof: &TpoProfile, cfg: tpo_builder::profile::SessionConfig| {
        let mut dates: Vec<NaiveDate> = rows.iter().map(|r| cfg.session_date(r.0)).collect();
        dates.dedup();
        let mut m = Tpo::new();
        for d in dates {
            let e = prof.entries_for_date(d);
            if !e.is_empty() {
                m.insert(d, e);
            }
        }
        m
    };
    Ok((by_date(&rth, rth_session_config()), by_date(&glx, overnight_session_config())))
}

fn composite(tpo: &Tpo, before: NaiveDate, n: usize) -> Option<ReferenceProfile> {
    let entries: Vec<TpoEntry> = tpo.range(..before).rev().take(n).flat_map(|(_, e)| e.iter().cloned()).collect();
    compute_metrics(&entries).map(|m| ReferenceProfile { poc: m.poc, vah: m.vah, val: m.val })
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    dotenvy::from_path("../bilore-ml-rs/.env").ok();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let from: NaiveDate = arg("--from").unwrap_or_else(|| "2026-03-13".into()).parse()?;
    let out = arg("--out").unwrap_or_else(|| "va_alert_study.csv".into());
    let url = std::env::var("DATABASE_URL")?;
    let pool = PgPoolOptions::new().max_connections(4).connect(&url).await?;
    let tick = Decimal::new(25, 2);

    // Coverage per session: (kind, start date, symbol) -> (first, last, minutes, ticks).
    let cov: Vec<(String, NaiveDate, String, NaiveTime, NaiveTime, i64, i64)> = sqlx::query_as(
        "WITH t AS (SELECT symbol, ts AT TIME ZONE 'America/Chicago' AS c FROM tick_trades WHERE symbol LIKE 'MES%' AND ts >= $1::date - 1), \
         s AS (SELECT symbol, c, CASE WHEN c::time >= '08:30' AND c::time < '15:00' AND extract(isodow FROM c) < 6 THEN 'rth' \
                                      WHEN c::time >= '17:00' THEN 'glx' WHEN c::time < '08:30' THEN 'glx' END AS kind, \
                       CASE WHEN c::time >= '08:30' THEN c::date ELSE c::date - 1 END AS d FROM t) \
         SELECT kind, d, symbol, min(c::time), max(c::time), count(DISTINCT date_trunc('minute', c)), count(*) FROM s \
         WHERE kind IS NOT NULL AND d >= $1 GROUP BY 1, 2, 3",
    )
    .bind(from)
    .fetch_all(&pool)
    .await?;
    let mut best = Coverage::new();
    for (kind, d, sym, first, last, minutes, n) in cov {
        let e = best.entry((kind, d)).or_insert((sym.clone(), first, last, minutes, 0));
        if n > e.4 {
            *e = (sym, first, last, minutes, n);
        }
    }
    let hm = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
    let mut sessions = Vec::new();
    for ((kind, d), (sym, first, last, minutes, _)) in &best {
        if kind == "rth" && fully_recorded(*first, *last, *minutes) {
            sessions.push(Session { kind: SessionKind::Rth, date: *d, symbol: sym.clone(), start: d.and_time(hm(8, 30)), end: d.and_time(hm(15, 0)) });
        } else if kind == "glx" && *minutes >= GLOBEX_MIN_MINUTES {
            // a Globex session's start date: Monday early belongs to Sunday 17:00
            let start = d.and_time(hm(17, 0));
            let Some(s) = va_alert::session_of(start) else { continue };
            sessions.push(Session { kind: SessionKind::Globex, date: s.start.date(), symbol: sym.clone(), start, end: start + Duration::minutes(930) });
        }
    }
    eprintln!("{} sessions ({} RTH, {} Globex)", sessions.len(), sessions.iter().filter(|s| s.kind == SessionKind::Rth).count(), sessions.iter().filter(|s| s.kind == SessionKind::Globex).count());

    let mut tpo: BTreeMap<String, (Tpo, Tpo)> = BTreeMap::new();
    let mut csv = std::fs::File::create(&out)?;
    let mut header = String::from(
        "kind,date,symbol,ts_ct,minute,period,sent,cand_first,side,fade,shape,price,poc,vah,val,va_width,range,beyond_edge,to_poc,\
         structure,pattern,order_flow,delta,otf,otf_class,d_loc,w_loc,m_loc,agreeing,\
         range30,move5,imb5,delta5,move15,imb15,delta15,large_buy,large_sell,\
         prior_poc,prior_vah,prior_val,prior_loc,prior_lean_agrees,\
         poc_s,mae_before_poc,mfe15,mae15,mfe30,mae30,mfe60,mae60,close_move",
    );
    for k in 1..=MAX_K {
        header.push_str(&format!(",fav{k}"));
    }
    for k in 1..=MAX_K {
        header.push_str(&format!(",adv{k}"));
    }
    writeln!(csv, "{header}")?;
    let (mut n_sent, mut n_rows) = (0, 0);

    for s in &sessions {
        if !tpo.contains_key(&s.symbol) {
            eprintln!("building tpo composites for {}", s.symbol);
            tpo.insert(s.symbol.clone(), build_tpo(&pool, &s.symbol).await?);
        }
        let (rth_tpo, glx_tpo) = &tpo[&s.symbol];
        let ttpo = if s.kind == SessionKind::Rth { rth_tpo } else { glx_tpo };
        let comp = match (composite(ttpo, s.date, 1), composite(ttpo, s.date, 5), composite(ttpo, s.date, 20)) {
            (Some(a), Some(b), Some(c)) => Some((a, b, c)),
            _ => None,
        };
        // prior RTH levels (the cockpit's fade lean): last RTH session of the contract before this one starts
        let prior_rth = composite(rth_tpo, if s.kind == SessionKind::Rth { s.date } else { s.date + Duration::days(1) }, 1);

        let rows: Vec<(DateTime<Utc>, Decimal, i32)> = sqlx::query_as(
            "SELECT ts, price, size FROM tick_trades WHERE symbol = $1 AND ts >= $2 AND ts < $3 ORDER BY ts",
        )
        .bind(&s.symbol)
        .bind(ct_to_utc(s.start))
        .bind(ct_to_utc(s.end))
        .fetch_all(&pool)
        .await?;
        let prices: Vec<(DateTime<Utc>, Decimal)> = rows.iter().map(|r| (r.0, r.1)).collect();

        let mut profile = DevelopingProfile::default();
        let mut bars = bilore_core::bar_builder::BarBuilder::new(5);
        let mut session_bars: Vec<Bar> = Vec::new();
        let mut classifier = TickRuleClassifier::default();
        let mut delta = 0i64;
        let (mut throttle, mut cand_throttle) = (AlertThrottle::default(), AlertThrottle::default());
        let mut last_price: Option<Decimal> = None;
        let mut window = Window::default();
        let (mut large_buy, mut large_sell) = (0usize, 0usize);
        let (mut hi, mut lo) = (Decimal::MIN, Decimal::MAX);

        for (i, &(ts, price, size)) in rows.iter().enumerate() {
            let size = size as i64;
            let ct = ts.with_timezone(&Chicago).naive_local();
            profile.on_trade(ct, price, size.max(0) as u64);
            if profile.session.is_none() || va_alert::session_of(ct) != profile.session {
                continue;
            }
            let t = Tick { ts, price, size };
            if let Some(bar) = bars.on_tick(&t) {
                session_bars.push(bar);
            }
            let side = classifier.classify(price);
            delta += match side {
                FlowSide::Buy => size,
                FlowSide::Sell => -size,
            };
            if size >= LARGE_PRINT_MIN {
                match side {
                    FlowSide::Buy => large_buy += 1,
                    FlowSide::Sell => large_sell += 1,
                }
            }
            window.push(ts, price, size, side);
            hi = hi.max(price);
            lo = lo.min(price);
            if let Some(poc) = profile.volume.poc() {
                throttle.observe(price, poc);
                cand_throttle.observe(price, poc);
            }
            if last_price == Some(price) {
                continue;
            }
            last_price = Some(price);
            let st = analyze_structure(&session_bars, Some(price), 3);
            let structure = structure_direction(&st);
            let view = comp.as_ref().map(|(d, w, m)| participant_view(d, w, m, price));
            let otf = view.as_ref().and_then(|v| v.lean);
            let factors = Factors { structure, delta, otf };
            let Some(c) = va_alert::candidate(&profile, ct, price, &factors, tick) else { continue };
            let sent = va_alert::evaluate(&profile, ct, price, &factors, tick).is_some() && throttle.should_send(&c);
            let first = cand_throttle.should_send(&c);
            if !sent && !first {
                continue;
            }
            n_rows += 1;
            n_sent += sent as usize;
            let o = outcome(&prices[i + 1..], ts, price, c.fade, c.poc);
            let (r30, _, _, _) = window.stats(ts, price, 30);
            let (_, m5, i5, d5) = window.stats(ts, price, 5);
            let (_, m15, i15, d15) = window.stats(ts, price, 15);
            let rd = |r: Read| match r {
                Read::Agree => 1,
                Read::Disagree => -1,
                Read::Neutral => 0,
            };
            let dir = |d: Direction| if d == Direction::Long { "long" } else { "short" };
            let beyond = if c.fade == Direction::Short { c.price - c.vah } else { c.val - c.price };
            let (pp, pvah, pval, ploc, plean) = match prior_rth {
                Some(p) => {
                    let loc = if price > p.vah { "above" } else if price < p.val { "below" } else { "inside" };
                    let lean = bilore_core::fade::fade_direction(price, p.poc) == c.fade;
                    (f(p.poc), f(p.vah), f(p.val), loc, (lean as i32).to_string())
                }
                None => (f64::NAN, f64::NAN, f64::NAN, "", String::new()),
            };
            let opt = |x: Option<f64>| x.map(|v| format!("{v:.1}")).unwrap_or_default();
            let mut line = format!(
                "{},{},{},{},{},{},{},{},{},{},{:?},{},{},{},{},{},{},{},{},{},{:?},{},{},{},{},{},{},{},{},{:.2},{:.2},{:.3},{},{:.2},{:.3},{},{},{},{:.2},{:.2},{:.2},{},{},{},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2}",
                if s.kind == SessionKind::Rth { "rth" } else { "globex" },
                s.date,
                s.symbol,
                ct.format("%Y-%m-%d %H:%M:%S"),
                (ct - s.start).num_minutes(),
                c.period,
                sent as i32,
                first as i32,
                if c.fade == Direction::Short { "upper" } else { "lower" },
                dir(c.fade),
                c.shape,
                c.price,
                c.poc,
                c.vah,
                c.val,
                f(c.vah - c.val),
                f(hi - lo),
                f(beyond),
                f((c.price - c.poc).abs()),
                rd(c.structure),
                st.pattern,
                rd(c.order_flow),
                c.delta,
                rd(c.otf),
                view.as_ref().map(|v| format!("{:?}", v.class)).unwrap_or_default(),
                view.as_ref().map(|v| v.daily_loc_score.to_string()).unwrap_or_default(),
                view.as_ref().map(|v| v.weekly_loc_score.to_string()).unwrap_or_default(),
                view.as_ref().map(|v| v.monthly_loc_score.to_string()).unwrap_or_default(),
                c.agreeing(),
                r30,
                m5,
                i5,
                d5,
                m15,
                i15,
                d15,
                large_buy,
                large_sell,
                pp,
                pvah,
                pval,
                ploc,
                plean,
                opt(o.poc_s),
                o.mae_before_poc,
                o.mfe[0],
                o.mae[0],
                o.mfe[1],
                o.mae[1],
                o.mfe[2],
                o.mae[2],
                o.close_move,
            );
            for v in o.fav.iter().chain(o.adv.iter()) {
                line.push(',');
                line.push_str(&opt(*v));
            }
            writeln!(csv, "{line}")?;
        }
        eprintln!("{:?} {} {}: {} ticks", s.kind, s.date, s.symbol, rows.len());
    }
    eprintln!("{n_rows} rows, {n_sent} alerts -> {out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    fn path(points: &[(i64, &str)]) -> (DateTime<Utc>, Vec<(DateTime<Utc>, Decimal)>) {
        let t0 = DateTime::parse_from_rfc3339("2026-10-26T15:00:00Z").unwrap().with_timezone(&Utc);
        (t0, points.iter().map(|(s, p)| (t0 + Duration::seconds(*s), d(p))).collect())
    }

    #[test]
    fn a_short_fade_records_first_passages_in_its_favour() {
        let (t0, after) = path(&[(10, "5011.00"), (20, "5008.00"), (30, "5004.00"), (40, "5006.00")]);
        let o = outcome(&after, t0, d("5010.00"), Direction::Short, d("5005.00"));
        assert_eq!(o.adv[0], Some(10.0), "1 point against at 5011");
        assert_eq!(o.adv[1], None);
        assert_eq!(o.fav[1], Some(20.0), "2 points for at 5008");
        assert_eq!(o.fav[5], Some(30.0), "6 points for at 5004");
        assert_eq!(o.poc_s, Some(30.0));
        assert_eq!(o.mae_before_poc, -1.0);
        assert_eq!(o.close_move, 4.0);
    }

    #[test]
    fn a_long_fade_that_never_reaches_the_poc_keeps_its_worst_move() {
        let (t0, after) = path(&[(60, "4998.00"), (2000, "4995.00"), (4000, "4999.00")]);
        let o = outcome(&after, t0, d("5000.00"), Direction::Long, d("5005.00"));
        assert_eq!(o.poc_s, None);
        assert_eq!(o.mae_before_poc, -5.0);
        assert_eq!((o.mae[0], o.mae[1], o.mae[2]), (-2.0, -2.0, -5.0), "15/30/60-minute worst");
        assert_eq!(o.close_move, -1.0);
    }
}
