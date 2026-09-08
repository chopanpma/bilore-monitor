//! Headless parallel monitor — generates its own live setups from
//! bilore-core's structure + order-flow confirmation (see `live_signal.rs`
//! and `bilore-backtest::signal`, whose logic this reuses unmodified),
//! independent of v1 and of `daily_plans` (which `bilore_session.py` has
//! silently stopped writing to since 2026-06-18 — found 2026-09-08, not
//! fixed here, this monitor sidesteps it entirely). Tracks one shadow
//! trade per session in `shadow_trades_v2`, sends `[V2]`-prefixed Telegram
//! alerts on the same bot/chat as v1, so the two streams can be compared
//! live. Runs independently of whether `bilore-cockpit`'s TUI is open —
//! auto-started by its supervisor like `tick_recorder`/`db-sink`/etc, or
//! runnable standalone via `cargo run --bin bilore-monitor`.

use anyhow::Result;
use bilore_backtest::signal::SignalConfig;
use bilore_core::confirmation::AlertConfig;
use bilore_core::shadow_trader::{self, Outcome, ShadowTrade};
use bilore_monitor::live_signal::{LiveMonitor, MonitorEvent};
use bilore_monitor::{db, telegram};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use chrono_tz::America::Chicago;
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;

struct OpenTrade {
    db_id: i64,
    trade: ShadowTrade,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Reads bilore-ml/.env directly (not this crate's own dir) — reuses
    // the same TELEGRAM_BOT_TOKEN/CHAT_ID and DATABASE_URL convention v1
    // already has configured, rather than requiring a second copy.
    let ml_env = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../bilore-ml/.env");
    dotenvy::from_path(&ml_env).ok();
    dotenvy::dotenv().ok();

    let log_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("logs/monitor.log");
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(log_file) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(log_file))
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".parse().unwrap()),
            )
            .try_init();
    }

    let symbol = std::env::var("SYMBOL").unwrap_or_else(|_| "MESU6".to_string());
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://bilore:bilore@localhost:5432/bilore".to_string());
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
    if bot_token.is_empty() || chat_id.is_empty() {
        tracing::warn!("TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID not set — alerts will be logged only, not sent");
    }

    let pool = PgPoolOptions::new().max_connections(3).connect(&db_url).await?;
    let http = reqwest::Client::new();

    let tick_size: Decimal = "0.25".parse().unwrap();
    let tick_value: Decimal = "1.25".parse().unwrap();
    let make_cfg = |now: DateTime<Utc>| SignalConfig {
        pivot_n: 3, // codebase default (matches bilore-cockpit's CockpitConfig / bilore-backtest)
        stop_buffer: "1".parse().unwrap(),
        target_r_multiple: "2".parse().unwrap(),
        scan_start_ts: Some(next_scan_start(now)),
    };
    let mut monitor = LiveMonitor::new(make_cfg(Utc::now()), AlertConfig::default());

    let mut last_bar_ts = Utc::now() - ChronoDuration::hours(12);
    let mut last_tick_ts = Utc::now() - ChronoDuration::hours(12);
    let mut current_ct_date: Option<NaiveDate> = Some(Utc::now().with_timezone(&Chicago).date_naive());
    let mut open_trade: Option<OpenTrade> = None;

    tracing::info!("bilore-monitor started, symbol={symbol}");

    let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick_interval.tick().await;

        let today_ct = Utc::now().with_timezone(&Chicago).date_naive();
        if current_ct_date != Some(today_ct) {
            if let Some(mut ot) = open_trade.take() {
                if shadow_trader::close(&mut ot.trade) {
                    let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                    notify(&http, &bot_token, &chat_id, &telegram::result_message(&symbol, &ot.trade)).await;
                }
            }
            monitor = LiveMonitor::new(make_cfg(Utc::now()), AlertConfig::default());
            current_ct_date = Some(today_ct);
            tracing::info!("session rollover -> {today_ct}");
        }

        match db::poll_new_bars(&pool, &symbol, last_bar_ts).await {
            Ok(bars) => {
                for bar in bars {
                    last_bar_ts = bar.ts;
                    if let Some(event) = monitor.on_bar(bar) {
                        handle_event(&pool, &http, &bot_token, &chat_id, &symbol, today_ct, &mut open_trade, event)
                            .await;
                    }
                }
            }
            Err(e) => tracing::error!("poll_new_bars failed: {e}"),
        }

        match db::poll_new_ticks(&pool, &symbol, last_tick_ts).await {
            Ok(ticks) => {
                for tick in ticks {
                    last_tick_ts = tick.ts;
                    if let Some(event) = monitor.on_tick(tick.clone()) {
                        handle_event(&pool, &http, &bot_token, &chat_id, &symbol, today_ct, &mut open_trade, event)
                            .await;
                    }

                    if let Some(ot) = open_trade.as_mut() {
                        if shadow_trader::on_price(&mut ot.trade, tick.price, tick_size, tick_value).is_some()
                            && matches!(ot.trade.outcome, Outcome::Won | Outcome::Lost)
                        {
                            let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                            notify(&http, &bot_token, &chat_id, &telegram::result_message(&symbol, &ot.trade)).await;
                        }
                    }
                }
            }
            Err(e) => tracing::error!("poll_new_ticks failed: {e}"),
        }
    }
}

async fn handle_event(
    pool: &sqlx::PgPool,
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    symbol: &str,
    today_ct: NaiveDate,
    open_trade: &mut Option<OpenTrade>,
    event: MonitorEvent,
) {
    match event {
        MonitorEvent::Unlocked => {
            tracing::info!("structure lean flipped — unlocked, watching for a new signal");
        }
        MonitorEvent::NewSignal(sig) => {
            if open_trade.is_some() {
                // Already tracking one trade this session — one setup/day,
                // matching v1's own "lock the first valid setup" semantics.
                return;
            }
            tracing::info!("new signal: {:?} entry={} stop={} target={} confirm={}",
                sig.direction, sig.entry_price, sig.stop_price, sig.target_price, sig.would_confirm);

            let mut trade = ShadowTrade::new(
                sig.direction,
                sig.entry_price,
                sig.entry_price,
                sig.stop_price,
                Some(sig.target_price),
                1,
                true,
            );
            // Immediate market-order-style entry — there's no zone to wait
            // for (unlike v1's daily_plans zone), the signal itself IS the
            // entry moment.
            trade.outcome = Outcome::Entered;
            trade.entry_price = Some(sig.entry_price);

            match db::insert_shadow_trade_v2(pool, today_ct, symbol, &trade).await {
                Ok(id) => *open_trade = Some(OpenTrade { db_id: id, trade }),
                Err(e) => tracing::error!("insert_shadow_trade_v2 failed: {e}"),
            }
            notify(http, bot_token, chat_id, &telegram::setup_message(symbol, &sig)).await;
        }
    }
}

async fn notify(http: &reqwest::Client, bot_token: &str, chat_id: &str, text: &str) {
    tracing::info!("{}", text.replace('\n', " | "));
    if bot_token.is_empty() || chat_id.is_empty() {
        return;
    }
    if let Err(e) = telegram::send(http, bot_token, chat_id, text).await {
        tracing::error!("telegram send failed: {e}");
    }
}

/// 06:00 CT of `now`'s own CT calendar day, in UTC — same reasoning as
/// `bilore-backtest`'s fresh-signal generator: pivots need only a handful
/// of bars, so scanning from midnight risks a "signal" firing on pure
/// noise in the near-zero-volume window right after CME's Sunday reopen,
/// before order_flow has accumulated enough ticks to ever confirm.
fn next_scan_start(now: DateTime<Utc>) -> DateTime<Utc> {
    let ct_date = now.with_timezone(&Chicago).date_naive();
    let midnight_ct = ct_date.and_hms_opt(0, 0, 0).unwrap();
    Chicago.from_local_datetime(&midnight_ct).single().unwrap().with_timezone(&Utc) + ChronoDuration::hours(6)
}
