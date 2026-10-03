//! One-off repair of `session_profiles` RTH rows (2026-10-02).
//!
//! tpo-builder's RTH session had no end until `baf159b`, so every live row
//! absorbed trades until the UTC date rolled (19:00 CT): post-close and
//! early-Globex volume shifted POC/VAH/VAL (10-01: true POC 7690, row 7730).
//! This rebuilds each row from that day's 08:30-15:00 CT ticks with the
//! monitor's own `levels::rth_levels_from_ticks` (same profile the strategies
//! now use), IB = 08:30-09:00 high/low. Days whose recording doesn't span the
//! session are left untouched and listed.
//!
//! Usage: `cargo run --release --bin repair_session_profiles -- [--symbol MESZ6] [--from 2026-09-15] [--apply]`
//! Dry run by default: prints old vs new; writes only with `--apply`.

use anyhow::Result;
use bilore_monitor::{db, levels};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let symbol = arg("--symbol").unwrap_or_else(|| "MESZ6".into());
    let from: NaiveDate = arg("--from").unwrap_or_else(|| "2026-09-15".into()).parse()?;
    let apply = args.iter().any(|a| a == "--apply");
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "postgres://bilore:bilore@localhost:5432/bilore".into());
    let pool = PgPoolOptions::new().max_connections(2).connect(&url).await?;

    let rows: Vec<(NaiveDate, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "SELECT date, poc::float8::numeric, vah::float8::numeric, val::float8::numeric FROM session_profiles \
         WHERE symbol = $1 AND date >= $2 AND date <= (now() AT TIME ZONE 'America/Chicago')::date ORDER BY date",
    )
    .bind(&symbol)
    .bind(from)
    .fetch_all(&pool)
    .await?;

    println!("{symbol} session_profiles from {from} ({}): {}", if apply { "APPLY" } else { "dry run" }, rows.len());
    println!("{:<12} {:>26}   {:>26}   action", "date", "old POC / VAH / VAL", "from ticks POC / VAH / VAL");
    let (mut changed, mut skipped) = (0, 0);
    for (date, old_poc, old_vah, old_val) in rows {
        let found = db::last_rth_volume_profile(&pool, &symbol, date.succ_opt().unwrap()).await?;
        let new = match found {
            Some((day, vols, first, last)) if day == date => levels::rth_levels_from_ticks(&vols, first, last),
            _ => None,
        };
        let Some(new) = new else {
            skipped += 1;
            println!("{date}   {old_poc:>8} {old_vah:>8} {old_val:>8}   {:>26}   skip: not fully recorded", "—");
            continue;
        };
        let ib: (Option<Decimal>, Option<Decimal>) = sqlx::query_as(
            "SELECT max(price), min(price) FROM tick_trades WHERE symbol = $1 \
               AND ts >= (($2::date + time '08:30')::timestamp AT TIME ZONE 'America/Chicago') \
               AND ts < (($2::date + time '09:00')::timestamp AT TIME ZONE 'America/Chicago')",
        )
        .bind(&symbol)
        .bind(date)
        .fetch_one(&pool)
        .await?;
        let same = new.poc == old_poc && new.vah == old_vah && new.val == old_val;
        println!(
            "{date}   {old_poc:>8} {old_vah:>8} {old_val:>8}   {:>8} {:>8} {:>8}   {}",
            new.poc,
            new.vah,
            new.val,
            if same { "unchanged" } else { "fix" }
        );
        if !same {
            changed += 1;
            if apply {
                sqlx::query(
                    "UPDATE session_profiles SET poc = $3, vah = $4, val = $5, \
                       ib_high = COALESCE($6, ib_high), ib_low = COALESCE($7, ib_low) \
                     WHERE symbol = $1 AND date = $2",
                )
                .bind(&symbol)
                .bind(date)
                .bind(new.poc)
                .bind(new.vah)
                .bind(new.val)
                .bind(ib.0)
                .bind(ib.1)
                .execute(&pool)
                .await?;
            }
        }
    }
    println!("\n{changed} row(s) {} · {skipped} skipped (not fully recorded)", if apply { "repaired" } else { "would change" });
    Ok(())
}
