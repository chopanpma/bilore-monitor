//! Polling + `shadow_trades_v2` persistence for the standalone monitor.
//! Deliberately self-contained rather than depending on
//! `bilore-cockpit::db` — that crate's `insert_shadow_trade_v2` is coupled
//! to a `DailyPlanRow` (from `daily_plans`, which this monitor doesn't use
//! at all — its setups come from `bilore-backtest::signal`, not a DB plan
//! row), and pulling in the whole `bilore-cockpit` crate (ratatui and all)
//! for a headless service isn't worth it just to reuse a few queries.

use anyhow::Result;
use bilore_core::market_structure::Bar as CoreBar;
use bilore_core::order_flow::Tick as CoreTick;
use bilore_core::shadow_trader::ShadowTrade;
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

#[derive(Debug, sqlx::FromRow)]
struct BarRow {
    ts: DateTime<Utc>,
    high: Decimal,
    low: Decimal,
    close: Decimal,
}

#[derive(Debug, sqlx::FromRow)]
struct TickRow {
    ts: DateTime<Utc>,
    price: Decimal,
    size: i32,
}

pub async fn poll_new_bars(pool: &PgPool, symbol: &str, since: DateTime<Utc>) -> Result<Vec<CoreBar>> {
    let rows = sqlx::query_as::<_, BarRow>(
        r#"
        SELECT ts, high::float8::numeric AS high, low::float8::numeric AS low, close::float8::numeric AS close
        FROM historical_bars
        WHERE symbol = $1 AND ts > $2
        ORDER BY ts
        "#,
    )
    .bind(symbol)
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| CoreBar { ts: r.ts, high: r.high, low: r.low, close: r.close }).collect())
}

pub async fn poll_new_ticks(pool: &PgPool, symbol: &str, since: DateTime<Utc>) -> Result<Vec<CoreTick>> {
    let rows = sqlx::query_as::<_, TickRow>(
        r#"
        SELECT ts, price, size
        FROM tick_trades
        WHERE symbol = $1 AND ts > $2
        ORDER BY ts
        "#,
    )
    .bind(symbol)
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| CoreTick { ts: r.ts, price: r.price, size: r.size as i64 }).collect())
}

/// Insert a fresh `shadow_trades_v2` row for a newly-locked signal.
/// Self-contained (no `DailyPlanRow`) — `anchor_lbl` is left NULL since
/// this monitor's setups don't come from a scored ML plan; `confidence`
/// carries the strategy's entry trust (0..1, signal-time — semantics per
/// strategy in `bilore-project-conf/contracts/strategies.md`, "Trust
/// metric": ml-model = leaned-side `risk_params.confidence`, fade-poc =
/// `bilore_core::fade::fade_trust`).
/// `strategy` is the `bilore_core::strategy` slug identifying which of the
/// monitor's strategies locked this trade (see
/// `bilore-project-conf/contracts/strategies.md`).
pub async fn insert_shadow_trade_v2(
    pool: &PgPool,
    session_date: NaiveDate,
    symbol: &str,
    strategy: &str,
    trust: Option<f64>,
    trade: &ShadowTrade,
) -> Result<i64> {
    let row: (i32,) = sqlx::query_as(
        r#"
        INSERT INTO shadow_trades_v2
            (session_date, symbol, strategy, direction, entry_lo, entry_hi, stop, target_1,
             stop_ticks, mes_contracts, confidence, outcome, signal_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NOW())
        RETURNING id
        "#,
    )
    .bind(session_date)
    .bind(symbol)
    .bind(strategy)
    .bind(format!("{:?}", trade.direction))
    .bind(trade.entry_lo)
    .bind(trade.entry_hi)
    .bind(trade.stop)
    .bind(trade.target_1)
    .bind((trade.entry_lo - trade.stop).abs() / Decimal::new(25, 2)) // stop_ticks, 0.25 tick size
    .bind(trade.mes_contracts)
    .bind(trust)
    .bind(trade.outcome.as_str())
    .fetch_one(pool)
    .await?;

    Ok(row.0 as i64)
}

pub async fn update_shadow_trade_v2(pool: &PgPool, id: i64, trade: &ShadowTrade) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE shadow_trades_v2
        SET    outcome     = $1,
               entry_price = $2,
               exit_price  = $3,
               pnl_ticks   = $4,
               pnl_dollars = $5,
               entry_at    = CASE WHEN $1 IN ('entered','won','lost') AND entry_at IS NULL
                                  THEN NOW() ELSE entry_at END,
               exit_at     = CASE WHEN $1 IN ('won','lost','expired','invalidated') AND exit_at IS NULL
                                  THEN NOW() ELSE exit_at END
        WHERE  id = $6
        "#,
    )
    .bind(trade.outcome.as_str())
    .bind(trade.entry_price)
    .bind(trade.exit_price)
    .bind(trade.pnl_ticks)
    .bind(trade.pnl_dollars)
    .bind(id as i32)
    .execute(pool)
    .await?;

    Ok(())
}

/// Free-text reason on a `shadow_trades_v2` row — used for why a pending
/// setup was invalidated (`notes` column, migration 010).
pub async fn set_shadow_trade_v2_notes(pool: &PgPool, id: i64, notes: &str) -> Result<()> {
    sqlx::query("UPDATE shadow_trades_v2 SET notes = $1 WHERE id = $2")
        .bind(notes)
        .bind(id as i32)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct SummaryRow {
    session_date: NaiveDate,
    symbol: String,
    strategy: String,
    outcome: String,
    pnl_ticks: Option<f64>,
    pnl_dollars: Option<f64>,
}

const SUMMARY_COLS: &str =
    "session_date, symbol, strategy, outcome, pnl_ticks::float8 AS pnl_ticks, pnl_dollars::float8 AS pnl_dollars";

/// `shadow_trades_v2` rows with `from <= session_date <= to`, only for
/// contracts of `roots` (`bilore_core::instrument::in_roots`) — feeds the
/// daily summary's Today/Week lines (`daily_summary::summary_message`).
pub async fn fetch_summary_rows(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    roots: &[&str],
) -> Result<Vec<crate::daily_summary::DayRow>> {
    let rows: Vec<SummaryRow> =
        sqlx::query_as(&format!("SELECT {SUMMARY_COLS} FROM shadow_trades_v2 WHERE session_date BETWEEN $1 AND $2"))
            .bind(from)
            .bind(to)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter(|r| bilore_core::instrument::in_roots(&r.symbol, roots))
        .map(|r| crate::daily_summary::DayRow {
            session_date: r.session_date,
            strategy: r.strategy,
            outcome: r.outcome,
            pnl_ticks: r.pnl_ticks,
            pnl_dollars: r.pnl_dollars,
        })
        .collect())
}

/// All resolved (`won`/`lost`) rows for contracts of `roots` — same scope
/// as `bilore-backtest-gate`'s default (`GATE_ROOTS`, MES), so the
/// summary's cumulative gate line matches that report.
pub async fn fetch_resolved_gate_rows(pool: &PgPool, roots: &[&str]) -> Result<Vec<bilore_backtest::gate::GateRow>> {
    let rows: Vec<SummaryRow> =
        sqlx::query_as(&format!("SELECT {SUMMARY_COLS} FROM shadow_trades_v2 WHERE outcome IN ('won', 'lost')"))
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .filter(|r| bilore_core::instrument::in_roots(&r.symbol, roots))
        .map(|r| bilore_backtest::gate::GateRow {
            strategy: r.strategy,
            outcome: r.outcome,
            pnl_ticks: r.pnl_ticks,
            pnl_dollars: r.pnl_dollars,
        })
        .collect())
}

/// An open (`pending`/`entered`) `shadow_trades_v2` row — restored into its
/// strategy slot on startup so a restart continues the trade instead of
/// duplicating it (`main.rs` `restore_open_trade`/`restore_slots`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OpenTradeRow {
    pub id: i32,
    pub strategy: String,
    pub direction: String,
    pub entry_lo: f64,
    pub entry_hi: f64,
    pub stop: f64,
    pub target_1: Option<f64>,
    pub entry_price: Option<f64>,
    pub outcome: String,
    pub confidence: Option<f64>,
    pub signal_at: DateTime<Utc>,
}

/// Today's open trades for one symbol, newest first.
pub async fn fetch_open_trades(pool: &PgPool, date: NaiveDate, symbol: &str) -> Result<Vec<OpenTradeRow>> {
    Ok(sqlx::query_as(
        "SELECT id, strategy, direction, entry_lo::float8 AS entry_lo, entry_hi::float8 AS entry_hi, \
                stop::float8 AS stop, target_1::float8 AS target_1, entry_price::float8 AS entry_price, \
                outcome, confidence::float8 AS confidence, signal_at \
         FROM shadow_trades_v2 \
         WHERE session_date = $1 AND symbol = $2 AND outcome IN ('pending', 'entered') \
         ORDER BY signal_at DESC",
    )
    .bind(date)
    .bind(symbol)
    .fetch_all(pool)
    .await?)
}

/// A trade stopped out today — its level is not re-taken in the same
/// direction (`main.rs` `stopped_levels_by_strategy`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoppedRow {
    pub strategy: String,
    pub direction: String,
    pub entry_lo: f64,
}

pub async fn fetch_stopped_levels(pool: &PgPool, date: NaiveDate, symbol: &str) -> Result<Vec<StoppedRow>> {
    Ok(sqlx::query_as(
        "SELECT strategy, direction, entry_lo::float8 AS entry_lo FROM shadow_trades_v2 \
         WHERE session_date = $1 AND symbol = $2 AND outcome = 'lost'",
    )
    .bind(date)
    .bind(symbol)
    .fetch_all(pool)
    .await?)
}

/// RTH volume at price for the latest trading date before `before` that has
/// RTH ticks for `symbol` (searching back a week), with that session's
/// first/last tick times (CT) — feeds `levels::rth_levels_from_ticks`.
pub async fn last_rth_volume_profile(
    pool: &PgPool,
    symbol: &str,
    before: NaiveDate,
) -> Result<Option<(NaiveDate, Vec<(Decimal, i64)>, chrono::NaiveTime, chrono::NaiveTime)>> {
    let day: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT max((ts AT TIME ZONE 'America/Chicago')::date) FROM tick_trades \
         WHERE symbol = $1 \
           AND ts >= (($2::date - 7)::timestamp AT TIME ZONE 'America/Chicago') \
           AND ts < ($2::date::timestamp AT TIME ZONE 'America/Chicago') \
           AND (ts AT TIME ZONE 'America/Chicago')::time >= '08:30' \
           AND (ts AT TIME ZONE 'America/Chicago')::time < '15:00'",
    )
    .bind(symbol)
    .bind(before)
    .fetch_one(pool)
    .await?;
    let Some(day) = day else { return Ok(None) };
    let window = "symbol = $1 \
         AND ts >= (($2::date + time '08:30')::timestamp AT TIME ZONE 'America/Chicago') \
         AND ts < (($2::date + time '15:00')::timestamp AT TIME ZONE 'America/Chicago')";
    let rows: Vec<(Decimal, i64)> =
        sqlx::query_as(&format!("SELECT price, sum(size)::bigint FROM tick_trades WHERE {window} GROUP BY price"))
            .bind(symbol)
            .bind(day)
            .fetch_all(pool)
            .await?;
    let span: (Option<chrono::NaiveTime>, Option<chrono::NaiveTime>) = sqlx::query_as(&format!(
        "SELECT min((ts AT TIME ZONE 'America/Chicago')::time), max((ts AT TIME ZONE 'America/Chicago')::time) FROM tick_trades WHERE {window}"
    ))
    .bind(symbol)
    .bind(day)
    .fetch_one(pool)
    .await?;
    Ok(match span {
        (Some(first), Some(last)) => Some((day, rows, first, last)),
        _ => None,
    })
}
