//! Headless parallel monitor — runs v1's REAL logic (predict -> risk_params
//! -> trade_setup, faithfully ported to `bilore-ml-rs`) live, driven by a
//! Rust-trained model instead of v1's sklearn one, independent of
//! `daily_plans` (which `bilore_session.py` silently stopped writing to
//! since 2026-06-18). Tracks one shadow trade per session in
//! `shadow_trades_v2`, sends `[V2]`-prefixed Telegram alerts on the same
//! bot/chat as v1, for a genuine LIVE comparison against v1's real
//! historical track record (61.3% win rate, `shadow_trades`) — after two
//! backtests of this same logic came back suspiciously strong (74-76% win
//! rate) and couldn't be fully verified clean, the decision was to trust
//! forward-only live results instead of chasing the backtest further.
//!
//! Superseded 2026-09-08's structure-only version (`live_signal.rs`,
//! `bilore-backtest::signal`) — that logic is left in place (still used by
//! `bilore-backtest`'s fresh-signal generator) but no longer drives this
//! binary.

use anyhow::Result;
use bilore_core::market_structure::Direction;
use bilore_core::shadow_trader::{self, Outcome, ShadowTrade};
use bilore_ml_rs::model::{Config as ModelConfig, LogisticModel};
use bilore_ml_rs::predict::{predict, LivePeriodInput};
use bilore_ml_rs::risk::{risk_params, ModelQuality, Probabilities, RiskConfig};
use bilore_ml_rs::trade_setup::{trade_setup, ProfileLevels, SetupResult, TradeSetupConfig};
use bilore_ml_rs::{db as ml_db, features};
use bilore_monitor::period_agg::{PeriodAggregator, PeriodBar};
use bilore_monitor::{db, sound, telegram};
use chrono::{Duration as ChronoDuration, NaiveDate, Utc};
use chrono_tz::America::Chicago;
use ndarray::Array1;
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;
use tpo_builder::profile::{rth_session_config, TpoProfile};

struct OpenTrade {
    db_id: i64,
    trade: ShadowTrade,
}

struct TrainedModel {
    models: Vec<LogisticModel>, // [p_bullish, p_break_vah, p_break_val, p_return_poc]
    quality: ModelQuality,
    means: Array1<f64>,
    stds: Array1<f64>,
}

/// Trains fresh from whatever `period_profiles`/`session_profiles`/
/// `historical_bars` data exists right now — matches v1's own convention
/// (`train_model()` retrains every script run, no saved model file). No
/// train/test split here (unlike `train`/`backtest-full`) — this is the
/// live path, not evaluation; use ALL available history.
async fn train(pool: &PgPool, symbol: &str) -> Result<TrainedModel> {
    let rows = ml_db::load_periods(pool, symbol).await?;
    anyhow::ensure!(rows.len() >= 20, "not enough period rows to train ({})", rows.len());

    let (mut x, y) = features::build_matrices(&rows);
    let (means, stds) = features::standardize(&mut x);
    let cfg = ModelConfig::default();

    let models: Vec<LogisticModel> =
        (0..features::N_TARGETS).map(|t| LogisticModel::train(&x, &y.column(t).to_owned(), &cfg)).collect();

    let degenerate = |t: usize| {
        let s: f64 = y.column(t).sum();
        s == 0.0 || s == y.nrows() as f64
    };
    let quality = ModelQuality {
        p_bullish: !degenerate(0),
        p_break_vah: !degenerate(1),
        p_break_val: !degenerate(2),
    };

    tracing::info!("trained on {} period rows for {symbol}", rows.len());
    Ok(TrainedModel { models, quality, means, stds })
}

/// Most recent PRIOR session's POC/VAH/VAL — the only reference levels
/// used, matching the look-ahead fix already applied to `bilore-ml-rs`.
async fn fetch_prior_session(pool: &PgPool, symbol: &str, before: NaiveDate) -> Result<Option<ProfileLevels>> {
    let row: Option<(Decimal, Decimal, Decimal)> = sqlx::query_as(
        "SELECT poc, vah, val FROM session_profiles WHERE symbol = $1 AND date < $2 ORDER BY date DESC LIMIT 1",
    )
    .bind(symbol)
    .bind(before)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(poc, vah, val)| ProfileLevels { poc, vah, val, ib_high: None, ib_low: None }))
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let risk_cfg = RiskConfig::default();
    let setup_cfg = TradeSetupConfig::default();

    let mut trained = train(&pool, &symbol).await?;
    let mut today_ct = Utc::now().with_timezone(&Chicago).date_naive();
    let mut prior = fetch_prior_session(&pool, &symbol, today_ct).await?;

    let mut tpo = TpoProfile::new(symbol.clone(), rth_session_config());
    let mut period_agg = PeriodAggregator::new(rth_session_config());

    // Running per-session state, updated as each period COMPLETES.
    let mut bull_count = 0u32;
    let mut period_count = 0u32;
    let mut cum_up = 0.0_f64;
    let mut cum_dn = 0.0_f64;
    let mut last_completed: Option<PeriodBar> = None;

    let mut last_bar_ts = Utc::now() - ChronoDuration::hours(12);
    let mut last_tick_ts = Utc::now() - ChronoDuration::hours(12);
    let mut open_trade: Option<OpenTrade> = None;
    let mut locked_today = false;

    tracing::info!("bilore-monitor started (ML pipeline), symbol={symbol}, prior_session={}", prior.is_some());

    let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick_interval.tick().await;

        let now_ct = Utc::now().with_timezone(&Chicago).date_naive();
        if now_ct != today_ct {
            if let Some(mut ot) = open_trade.take() {
                if shadow_trader::close(&mut ot.trade) {
                    let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                    sound::play(sound::AlertKind::Expired);
                    notify(&http, &bot_token, &chat_id, &telegram::result_message(&symbol, &ot.trade)).await;
                }
            }
            today_ct = now_ct;
            prior = fetch_prior_session(&pool, &symbol, today_ct).await.unwrap_or(None);
            tpo = TpoProfile::new(symbol.clone(), rth_session_config());
            period_agg = PeriodAggregator::new(rth_session_config());
            bull_count = 0;
            period_count = 0;
            cum_up = 0.0;
            cum_dn = 0.0;
            last_completed = None;
            locked_today = false;
            match train(&pool, &symbol).await {
                Ok(t) => trained = t,
                Err(e) => tracing::error!("retrain failed, keeping yesterday's model: {e}"),
            }
            tracing::info!("session rollover -> {today_ct}");
        }

        if let Ok(bars) = db::poll_new_bars(&pool, &symbol, last_bar_ts).await {
            if let Some(last) = bars.last() {
                last_bar_ts = last.ts;
            }
        }

        let ticks = match db::poll_new_ticks(&pool, &symbol, last_tick_ts).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("poll_new_ticks failed: {e}");
                continue;
            }
        };

        for tick in &ticks {
            last_tick_ts = tick.ts;
            tpo.add_trade(tick.price, tick.size.max(0) as u64, tick.ts);

            if let Some(ot) = open_trade.as_mut() {
                if shadow_trader::on_price(&mut ot.trade, tick.price, tick_size, tick_value).is_some()
                    && matches!(ot.trade.outcome, Outcome::Won | Outcome::Lost)
                {
                    let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                    sound::play(if ot.trade.outcome == Outcome::Won { sound::AlertKind::Won } else { sound::AlertKind::Lost });
                    notify(&http, &bot_token, &chat_id, &telegram::result_message(&symbol, &ot.trade)).await;
                }
            }

            if let Some(finished) = period_agg.on_tick(tick) {
                cum_up += finished.up_vol as f64;
                cum_dn += finished.down_vol as f64;
                bull_count += u32::from(finished.close > finished.open);
                period_count += 1;
                last_completed = Some(finished);

                if !locked_today && open_trade.is_none() {
                    if let Some(prior_levels) = &prior {
                        try_signal(
                            &pool, &http, &bot_token, &chat_id, &symbol, today_ct,
                            &trained, &tpo, prior_levels, &last_completed, bull_count, period_count,
                            cum_up, cum_dn, &risk_cfg, &setup_cfg, &mut open_trade, &mut locked_today,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_signal(
    pool: &PgPool,
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    symbol: &str,
    today_ct: NaiveDate,
    trained: &TrainedModel,
    tpo: &TpoProfile,
    prior_levels: &ProfileLevels,
    last_completed: &Option<PeriodBar>,
    bull_count: u32,
    period_count: u32,
    cum_up: f64,
    cum_dn: f64,
    risk_cfg: &RiskConfig,
    setup_cfg: &TradeSetupConfig,
    open_trade: &mut Option<OpenTrade>,
    locked_today: &mut bool,
) {
    let Some(prev) = last_completed else { return };
    let next_period_idx = prev.period_idx + 1;
    if next_period_idx > 12 {
        return; // past the 13-period RTH cap (A-M)
    }

    let metrics = tpo.metrics_for_date(today_ct);
    let (vah, val, ib_high, ib_low) = match &metrics {
        Some(m) => (m.vah, m.val, m.ib_high, m.ib_low),
        None => return, // no trades recorded yet today — nothing to score against
    };

    let prev_tot = (prev.up_vol + prev.down_vol) as f64;
    let prev_delta_ratio = if prev_tot > 0.0 { (prev.up_vol - prev.down_vol) as f64 / prev_tot } else { 0.0 };
    let cum_tot = cum_up + cum_dn;
    let cum_delta_ratio = if cum_tot > 0.0 { (cum_up - cum_dn) / cum_tot } else { 0.0 };
    let bull_frac = if period_count > 0 { bull_count as f64 / period_count as f64 } else { 0.0 };

    let input = LivePeriodInput {
        period_idx: next_period_idx,
        open: prev.close, // best available "current price" — the new period's own open isn't known until it prints a tick
        vah,
        val,
        ib_high,
        ib_low,
        ref_poc: prior_levels.poc,
        ref_vah: prior_levels.vah,
        ref_val: prior_levels.val,
        prev_bullish: prev.close > prev.open,
        prev_range: prev.high - prev.low,
        prev_volume: prev.volume,
        bull_frac,
        prev_delta_ratio,
        cum_delta_ratio,
    };

    let probs: Probabilities = predict(&trained.models, &trained.means, &trained.stds, &input);

    let r_long = risk_params(Direction::Long, &probs, 1, &trained.quality, risk_cfg, 1.25);
    let r_short = risk_params(Direction::Short, &probs, 1, &trained.quality, risk_cfg, 1.25);
    let (direction, lean_risk) =
        if r_long.confidence >= r_short.confidence { (Direction::Long, r_long) } else { (Direction::Short, r_short) };

    let result = trade_setup(direction, prev.close, prior_levels, &lean_risk, None, setup_cfg);
    let setup = match result {
        SetupResult::Setup(s) => s,
        SetupResult::NoSetup(reason) => {
            tracing::debug!("no setup for period {next_period_idx}: {reason}");
            return;
        }
    };

    tracing::info!(
        "LOCKED: {:?} entry={} stop={} target={} confidence={:.2} tier={}",
        setup.direction, setup.entry_price, setup.stop, setup.targets[0].price, lean_risk.confidence, lean_risk.tier_label
    );

    let mut trade = ShadowTrade::new(direction, setup.entry_price, setup.entry_price, setup.stop, Some(setup.targets[0].price), 1, true);
    trade.outcome = Outcome::Entered;
    trade.entry_price = Some(setup.entry_price);

    match db::insert_shadow_trade_v2(pool, today_ct, symbol, &trade).await {
        Ok(id) => *open_trade = Some(OpenTrade { db_id: id, trade }),
        Err(e) => tracing::error!("insert_shadow_trade_v2 failed: {e}"),
    }
    *locked_today = true;

    sound::play(sound::AlertKind::Setup);
    let msg = format!(
        "🟢 <b>[V2] SETUP — {symbol}</b>\nDirection : <b>{:?}</b>\nEntry     : <b>{:.2}</b>\nStop      : {:.2}\nTarget    : {:.2}\nConfidence: {:.0}% ({})",
        setup.direction, setup.entry_price, setup.stop, setup.targets[0].price, lean_risk.confidence * 100.0, lean_risk.tier_label
    );
    notify(http, bot_token, chat_id, &msg).await;
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
