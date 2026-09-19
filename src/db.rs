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
/// `fade::fade_trust`).
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
               exit_at     = CASE WHEN $1 IN ('won','lost','expired') AND exit_at IS NULL
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
