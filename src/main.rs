//! Headless parallel monitor — runs v1's REAL logic (predict -> risk_params
//! -> trade_setup, faithfully ported to `bilore-ml-rs`) live, driven by a
//! Rust-trained model instead of v1's sklearn one, independent of
//! `daily_plans` (which `bilore_session.py` silently stopped writing to
//! since 2026-06-18). Tracks shadow trades in `shadow_trades_v2`, sends
//! `[V2]`-prefixed Telegram alerts on the same bot/chat as v1, for a
//! genuine LIVE comparison against v1's real historical track record (61.3%
//! win rate, `shadow_trades`) — after two backtests of this same logic came
//! back suspiciously strong (74-76% win rate) and couldn't be fully
//! verified clean, the decision was to trust forward-only live results
//! instead of chasing the backtest further.
//!
//! **Two strategies per symbol, independent (2026-09-18):** the original ML
//! pipeline (`ml-model`) plus the fade-toward-POC rule (`fade-poc`,
//! model-analysis.md §6.10's live lead — see `fade.rs`). Each has its own
//! open-trade slot and one-trade-per-session lock, so neither suppresses
//! the other's signals; every `shadow_trades_v2` row and Telegram alert
//! carries its strategy label (`bilore_core::strategy`,
//! contracts/strategies.md).
//!
//! Superseded 2026-09-08's structure-only version (`live_signal.rs`,
//! `bilore-backtest::signal`) — that logic is left in place (still used by
//! `bilore-backtest`'s fresh-signal generator) but no longer drives this
//! binary.
//!
//! **Multi-symbol (2026-09-16):** one process now serves every symbol in
//! `SYMBOLS` (comma-separated env, same convention as `bilore-tick-bridge`/
//! `bilore-cockpit`; `SYMBOL` is always included and always primary), each
//! with its own `PerSymbolState` — own model, own TPO profile, own
//! period-aggregator, own open shadow trade. A symbol that can't train yet
//! (not enough real-volume sessions — see `bilore_ml_rs::db`'s
//! `real_volume_cutoff`, e.g. MNQZ6/NQZ6 right after their tick backfill
//! starts) is skipped at startup rather than crashing the whole process,
//! and retried every ~60s until it has enough data — so a freshly-added
//! symbol comes online on its own once its backfill catches up, no restart
//! needed. `BRIDGE_FROM_SYMBOL` (set during a contract roll, e.g.
//! MESU6->MESZ6) applies ONLY to the primary `SYMBOL` — it must NOT leak
//! into other registered symbols' training, which have no relationship to
//! whatever contract the primary happens to be bridging from.

use anyhow::Result;
use bilore_core::market_structure::Direction;
use bilore_core::shadow_trader::{self, Outcome, ShadowTrade};
use bilore_core::strategy;
use bilore_ml_rs::model::{Config as ModelConfig, LogisticModel};
use bilore_ml_rs::predict::{feature_vector, predict, LivePeriodInput};
use bilore_ml_rs::risk::{risk_params, ModelQuality, Probabilities, RiskConfig};
use bilore_ml_rs::trade_setup::{trade_setup, ProfileLevels, SetupResult, TradeSetupConfig};
use bilore_ml_rs::{db as ml_db, features};
use bilore_ml_rs::live_state::{LiveModelState, LiveSetup};
use bilore_monitor::fade;
use bilore_monitor::period_agg::{PeriodAggregator, PeriodBar};
use bilore_monitor::{db, sound, telegram};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, Utc};
use chrono_tz::America::Chicago;
use ndarray::Array1;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;
use tpo_builder::profile::{rth_session_config, TpoProfile};

struct OpenTrade {
    db_id: i64,
    trade: ShadowTrade,
    /// Entry trust (0..1) captured at lock time — the same value persisted
    /// to `shadow_trades_v2.confidence`, echoed back in the result
    /// Telegram message (contracts/strategies.md, "Trust metric").
    trust: f64,
}

/// One strategy's shadow-trade slot within a symbol's state: its currently
/// tracked trade and its own one-trade-per-session lock. Two slots
/// (`ml-model`, `fade-poc`) run fully independent of each other so neither
/// strategy can suppress the other's signals — see
/// `bilore-project-conf/contracts/strategies.md`.
#[derive(Default)]
struct StrategySlot {
    open: Option<OpenTrade>,
    locked_today: bool,
}

struct TrainedModel {
    models: Vec<LogisticModel>, // [p_bullish, p_break_vah, p_break_val, p_return_poc]
    quality: ModelQuality,
    means: Array1<f64>,
    stds: Array1<f64>,
}

/// Everything the monitor tracks for one symbol, so one process can serve
/// several at once (see module doc comment). Built once at startup (or on
/// a pending-symbol retry) via `init_symbol_state`, then mutated in place
/// each tick.
struct PerSymbolState {
    trained: TrainedModel,
    today_ct: NaiveDate,
    prior: Option<ProfileLevels>,
    prior_hilo: Option<(Decimal, Decimal)>, // (high, low) — see fetch_prior_hilo
    tpo: TpoProfile,
    period_agg: PeriodAggregator,
    bull_count: u32,
    period_count: u32,
    cum_up: f64,
    cum_dn: f64,
    last_completed: Option<PeriodBar>,
    /// Every period that has completed so far today, in order — feeds
    /// `market_structure::find_pivots` for `dist_rally_high`/
    /// `dist_pullback_low` (2026-09-16). By construction this only ever
    /// contains periods strictly before whichever one is about to be
    /// scored (a period is pushed here at the same moment `last_completed`
    /// is set, before the NEXT period has printed any ticks), matching
    /// `features::build_matrices`' `&session[..k]` look-ahead discipline
    /// exactly — same property, live side.
    swing_bars: Vec<bilore_core::market_structure::Bar>,
    last_bar_ts: DateTime<Utc>,
    last_tick_ts: DateTime<Utc>,
    /// `ml-model` slot — the ML pipeline's shadow trade (see `try_signal`).
    ml: StrategySlot,
    /// `fade-poc` slot — the fade-toward-POC rule's shadow trade
    /// (see `try_fade_signal`, `bilore_monitor::fade`).
    fade: StrategySlot,
}

/// Trains fresh from whatever `period_profiles`/`session_profiles`/
/// `historical_bars` data exists right now — matches v1's own convention
/// (`train_model()` retrains every script run, no saved model file). No
/// train/test split here (unlike `train`/`backtest-full`) — this is the
/// live path, not evaluation; use ALL available history.
async fn train(pool: &PgPool, symbol: &str, bridge_from: Option<&str>) -> Result<TrainedModel> {
    let rows = match bridge_from {
        Some(bf) if !bf.is_empty() => {
            tracing::info!("training bridged: {symbol} + {bf}");
            ml_db::load_periods_bridged(pool, symbol, bf).await?
        }
        _ => ml_db::load_periods(pool, symbol).await?,
    };
    // Session count, not raw row count — a real floor. 20 PERIOD rows is
    // only ~1.5 trading days; MNQZ6 crossed that within days of its own
    // historical_bars backfill catching up and produced a degenerate model
    // (z-scores in the billions, probabilities pinned at 0/1 — see
    // model-analysis.md's MNQZ6 Backlog entry, 2026-09-16). `bilore-ml-rs`'s
    // own CLAUDE.md already documents "30+ sessions" as the real usability
    // floor; this enforces it instead of a magic row count that varies with
    // periods-per-day.
    let session_count = rows.iter().map(|r| r.date).collect::<std::collections::HashSet<_>>().len();
    anyhow::ensure!(session_count >= 30, "not enough real-volume sessions to train ({session_count}, need 30+)");

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
/// Falls back to `bridge_from`'s last session when `symbol` has none of
/// its own yet (a freshly-rolled contract's first day) — same rationale
/// as `load_periods_bridged`: front-month ES/MES contracts trade at nearly
/// the same price, so the just-expired contract's last close is a
/// reasonable stand-in for one day rather than generating no signal at all.
async fn fetch_prior_session(
    pool: &PgPool,
    symbol: &str,
    before: NaiveDate,
    bridge_from: Option<&str>,
) -> Result<Option<ProfileLevels>> {
    async fn query(pool: &PgPool, symbol: &str, before: NaiveDate) -> Result<Option<(Decimal, Decimal, Decimal)>> {
        Ok(sqlx::query_as(
            "SELECT poc, vah, val FROM session_profiles WHERE symbol = $1 AND date < $2 ORDER BY date DESC LIMIT 1",
        )
        .bind(symbol)
        .bind(before)
        .fetch_optional(pool)
        .await?)
    }

    let row = match query(pool, symbol, before).await? {
        Some(row) => Some(row),
        None => match bridge_from {
            Some(bf) if !bf.is_empty() => query(pool, bf, before).await?,
            _ => None,
        },
    };

    Ok(row.map(|(poc, vah, val)| ProfileLevels { poc, vah, val, ib_high: None, ib_low: None }))
}

/// Prior session's own RTH high/low (2026-09-16) — same "most recent
/// session before `before`, with `bridge_from` fallback" shape as
/// `fetch_prior_session`, but from `historical_bars` since neither
/// `session_profiles` (no high/low column) nor `tpo_bars` (only 7 days of
/// real history — checked, not enough) has it. Feeds
/// `predict::LivePeriodInput::ref_high`/`ref_low` — see `features.rs`'s
/// doc comment on `bilore-ml-rs` for why these are a different kind of
/// feature from `prior_poc`/`vah`/`val`, not just more of the same.
async fn fetch_prior_hilo(
    pool: &PgPool,
    symbol: &str,
    before: NaiveDate,
    bridge_from: Option<&str>,
) -> Result<Option<(Decimal, Decimal)>> {
    async fn query(pool: &PgPool, symbol: &str, before: NaiveDate) -> Result<Option<(Decimal, Decimal)>> {
        let date: Option<NaiveDate> = sqlx::query_scalar(
            "SELECT date FROM session_profiles WHERE symbol = $1 AND date < $2 ORDER BY date DESC LIMIT 1",
        )
        .bind(symbol)
        .bind(before)
        .fetch_optional(pool)
        .await?;
        let Some(date) = date else { return Ok(None) };

        let row: Option<(Option<Decimal>, Option<Decimal>)> = sqlx::query_as(
            r#"
            SELECT MAX(high)::float8::numeric, MIN(low)::float8::numeric
            FROM historical_bars
            WHERE symbol = $1
              AND (ts AT TIME ZONE 'America/Chicago')::date = $2
              AND (ts AT TIME ZONE 'America/Chicago')::time >= TIME '08:30:00'
              AND (ts AT TIME ZONE 'America/Chicago')::time <  TIME '15:00:00'
            "#,
        )
        .bind(symbol)
        .bind(date)
        .fetch_optional(pool)
        .await?;
        Ok(row.and_then(|(h, l)| h.zip(l)))
    }

    let result = match query(pool, symbol, before).await? {
        Some(r) => Some(r),
        None => match bridge_from {
            Some(bf) if !bf.is_empty() => query(pool, bf, before).await?,
            _ => None,
        },
    };
    Ok(result)
}

async fn init_symbol_state(pool: &PgPool, symbol: &str, bridge_from: Option<&str>, today_ct: NaiveDate) -> Result<PerSymbolState> {
    let trained = train(pool, symbol, bridge_from).await?;
    let prior = fetch_prior_session(pool, symbol, today_ct, bridge_from).await?;
    let prior_hilo = fetch_prior_hilo(pool, symbol, today_ct, bridge_from).await?;
    let start = Utc::now() - ChronoDuration::hours(12);
    Ok(PerSymbolState {
        trained,
        today_ct,
        prior,
        prior_hilo,
        tpo: TpoProfile::new(symbol.to_string(), rth_session_config()),
        period_agg: PeriodAggregator::new(rth_session_config()),
        bull_count: 0,
        period_count: 0,
        cum_up: 0.0,
        cum_dn: 0.0,
        last_completed: None,
        swing_bars: Vec::new(),
        last_bar_ts: start,
        last_tick_ts: start,
        ml: StrategySlot::default(),
        fade: StrategySlot::default(),
    })
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

    let primary_symbol = std::env::var("SYMBOL").unwrap_or_else(|_| "MESU6".to_string());
    let mut registered: Vec<String> = std::env::var("SYMBOLS")
        .ok()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    if !registered.contains(&primary_symbol) {
        registered.insert(0, primary_symbol.clone());
    }
    let bridge_from_env = std::env::var("BRIDGE_FROM_SYMBOL").ok().filter(|s| !s.is_empty());
    // BRIDGE_FROM_SYMBOL describes the primary symbol's own just-expired
    // contract (e.g. MESU6 for MESZ6) — it has no meaning for any other
    // registered symbol, so it must not be handed to them.
    let bridge_from_for = |symbol: &str| -> Option<String> {
        if symbol == primary_symbol { bridge_from_env.clone() } else { None }
    };

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://bilore:bilore@localhost:5432/bilore".to_string());
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
    if bot_token.is_empty() || chat_id.is_empty() {
        tracing::warn!("TELEGRAM_BOT_TOKEN/TELEGRAM_CHAT_ID not set — alerts will be logged only, not sent");
    }

    let pool = PgPoolOptions::new().max_connections(3 + registered.len() as u32).connect(&db_url).await?;
    let http = reqwest::Client::new();

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats = match async_nats::connect(&nats_url).await {
        Ok(nc) => Some(nc),
        Err(e) => {
            tracing::warn!("NATS connect failed ({e}) — live model state won't be published");
            None
        }
    };

    let tick_size: Decimal = "0.25".parse().unwrap();
    let tick_value: Decimal = "1.25".parse().unwrap();
    let risk_cfg = RiskConfig::default();
    let setup_cfg = TradeSetupConfig::default();

    let today_ct0 = Utc::now().with_timezone(&Chicago).date_naive();
    let mut states: HashMap<String, PerSymbolState> = HashMap::new();
    let mut pending: Vec<String> = Vec::new();
    for symbol in &registered {
        let bf = bridge_from_for(symbol);
        match init_symbol_state(&pool, symbol, bf.as_deref(), today_ct0).await {
            Ok(s) => {
                tracing::info!("{symbol} ready, prior_session={}", s.prior.is_some());
                states.insert(symbol.clone(), s);
            }
            Err(e) => {
                tracing::warn!("{symbol} not ready yet ({e}) — will retry periodically");
                pending.push(symbol.clone());
            }
        }
    }
    anyhow::ensure!(
        !states.is_empty() || !pending.is_empty(),
        "SYMBOLS/SYMBOL resolved to no symbols to monitor"
    );
    tracing::info!(
        "bilore-monitor started (ML pipeline), registered={:?}, ready={:?}, pending={:?}",
        registered,
        states.keys().collect::<Vec<_>>(),
        pending
    );

    let mut last_pending_retry = Utc::now();
    let mut tick_interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick_interval.tick().await;

        if !pending.is_empty() && (Utc::now() - last_pending_retry) >= ChronoDuration::seconds(60) {
            last_pending_retry = Utc::now();
            let today_ct = Utc::now().with_timezone(&Chicago).date_naive();
            let mut still_pending = Vec::new();
            for symbol in pending.drain(..) {
                let bf = bridge_from_for(&symbol);
                match init_symbol_state(&pool, &symbol, bf.as_deref(), today_ct).await {
                    Ok(s) => {
                        tracing::info!("{symbol} now has enough data — activating");
                        states.insert(symbol, s);
                    }
                    Err(_) => still_pending.push(symbol),
                }
            }
            pending = still_pending;
        }

        for (symbol, state) in states.iter_mut() {
            let bf = bridge_from_for(symbol);
            let now_ct = Utc::now().with_timezone(&Chicago).date_naive();
            if now_ct != state.today_ct {
                if let Some(mut ot) = state.ml.open.take() {
                    if shadow_trader::close(&mut ot.trade) {
                        let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                        sound::play(sound::AlertKind::Expired);
                        notify(&http, &bot_token, &chat_id, &telegram::result_message(symbol, strategy::ML_MODEL, &ot.trade, ot.trust)).await;
                    }
                }
                if let Some(mut ot) = state.fade.open.take() {
                    if shadow_trader::close(&mut ot.trade) {
                        let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                        sound::play(sound::AlertKind::Expired);
                        notify(&http, &bot_token, &chat_id, &telegram::result_message(symbol, strategy::FADE_POC, &ot.trade, ot.trust)).await;
                    }
                }
                state.today_ct = now_ct;
                state.prior = fetch_prior_session(&pool, symbol, now_ct, bf.as_deref()).await.unwrap_or(None);
                state.prior_hilo = fetch_prior_hilo(&pool, symbol, now_ct, bf.as_deref()).await.unwrap_or(None);
                state.tpo = TpoProfile::new(symbol.clone(), rth_session_config());
                state.period_agg = PeriodAggregator::new(rth_session_config());
                state.bull_count = 0;
                state.period_count = 0;
                state.cum_up = 0.0;
                state.cum_dn = 0.0;
                state.last_completed = None;
                state.swing_bars.clear();
                state.ml = StrategySlot::default();
                state.fade = StrategySlot::default();
                match train(&pool, symbol, bf.as_deref()).await {
                    Ok(t) => state.trained = t,
                    Err(e) => tracing::error!("{symbol}: retrain failed, keeping yesterday's model: {e}"),
                }
                tracing::info!("{symbol}: session rollover -> {now_ct}");
            }

            if let Ok(bars) = db::poll_new_bars(&pool, symbol, state.last_bar_ts).await {
                if let Some(last) = bars.last() {
                    state.last_bar_ts = last.ts;
                }
            }

            let ticks = match db::poll_new_ticks(&pool, symbol, state.last_tick_ts).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!("{symbol}: poll_new_ticks failed: {e}");
                    continue;
                }
            };

            for tick in &ticks {
                state.last_tick_ts = tick.ts;
                state.tpo.add_trade(tick.price, tick.size.max(0) as u64, tick.ts);

                for (slot, slug) in [(&mut state.ml, strategy::ML_MODEL), (&mut state.fade, strategy::FADE_POC)] {
                    if let Some(ot) = slot.open.as_mut() {
                        if shadow_trader::on_price(&mut ot.trade, tick.price, tick_size, tick_value).is_some()
                            && matches!(ot.trade.outcome, Outcome::Won | Outcome::Lost)
                        {
                            let _ = db::update_shadow_trade_v2(&pool, ot.db_id, &ot.trade).await;
                            sound::play(if ot.trade.outcome == Outcome::Won { sound::AlertKind::Won } else { sound::AlertKind::Lost });
                            notify(&http, &bot_token, &chat_id, &telegram::result_message(symbol, slug, &ot.trade, ot.trust)).await;
                        }
                    }
                }

                if let Some(finished) = state.period_agg.on_tick(tick) {
                    state.cum_up += finished.up_vol as f64;
                    state.cum_dn += finished.down_vol as f64;
                    state.bull_count += u32::from(finished.close > finished.open);
                    state.period_count += 1;
                    state.last_completed = Some(finished);
                    state.swing_bars.push(bilore_core::market_structure::Bar {
                        ts: tick.ts,
                        high: finished.high,
                        low: finished.low,
                        close: finished.close,
                    });

                    if state.prior.is_some() {
                        if !state.ml.locked_today && state.ml.open.is_none() {
                            try_signal(
                                &pool, &http, &bot_token, &chat_id, nats.as_ref(), symbol,
                                &risk_cfg, &setup_cfg, state,
                            )
                            .await;
                        }
                        if !state.fade.locked_today && state.fade.open.is_none() {
                            try_fade_signal(
                                &pool, &http, &bot_token, &chat_id, nats.as_ref(), symbol,
                                &risk_cfg, &setup_cfg, state,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }
}

/// Builds the live feature input for the NEXT period from state accumulated
/// so far today. Shared by both strategies — the fade arm still feeds the
/// trained model's probabilities into `risk_params` for sizing, exactly as
/// the validated `backtest_tick_sim_direction.rs` fade arm did, so it needs
/// the same input.
fn build_live_input(
    state: &PerSymbolState,
    prior_levels: &ProfileLevels,
    ib_high: Option<Decimal>,
    ib_low: Option<Decimal>,
    prev: &PeriodBar,
) -> LivePeriodInput {
    let prev_tot = (prev.up_vol + prev.down_vol) as f64;
    let prev_delta_ratio = if prev_tot > 0.0 { (prev.up_vol - prev.down_vol) as f64 / prev_tot } else { 0.0 };
    let cum_tot = state.cum_up + state.cum_dn;
    let cum_delta_ratio = if cum_tot > 0.0 { (state.cum_up - state.cum_dn) / cum_tot } else { 0.0 };
    let bull_frac = if state.period_count > 0 { state.bull_count as f64 / state.period_count as f64 } else { 0.0 };

    let (swing_highs, swing_lows) =
        bilore_core::market_structure::find_pivots(&state.swing_bars, bilore_ml_rs::features::SWING_PIVOT_N);
    LivePeriodInput {
        period_idx: prev.period_idx + 1,
        open: prev.close, // best available "current price" — the new period's own open isn't known until it prints a tick
        ib_high,
        ib_low,
        ref_poc: prior_levels.poc,
        ref_vah: prior_levels.vah,
        ref_val: prior_levels.val,
        ref_high: state.prior_hilo.map(|(h, _)| h).unwrap_or(Decimal::ZERO),
        ref_low: state.prior_hilo.map(|(_, l)| l).unwrap_or(Decimal::ZERO),
        prev_bullish: prev.close > prev.open,
        prev_range: prev.high - prev.low,
        prev_volume: prev.volume,
        bull_frac,
        prev_delta_ratio,
        cum_delta_ratio,
        nearest_rally_high: swing_highs.last().map(|p| p.price),
        nearest_pullback_low: swing_lows.last().map(|p| p.price),
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_signal(
    pool: &PgPool,
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    nats: Option<&async_nats::Client>,
    symbol: &str,
    risk_cfg: &RiskConfig,
    setup_cfg: &TradeSetupConfig,
    state: &mut PerSymbolState,
) {
    let Some(prev) = state.last_completed else { return };
    let next_period_idx = prev.period_idx + 1;
    if next_period_idx > 12 {
        return; // past the 13-period RTH cap (A-M)
    }

    let Some(prior_levels) = state.prior else { return };

    let metrics = state.tpo.metrics_for_date(state.today_ct);
    let (ib_high, ib_low) = match &metrics {
        Some(m) => (m.ib_high, m.ib_low),
        None => return, // no trades recorded yet today — nothing to score against
    };

    let input = build_live_input(state, &prior_levels, ib_high, ib_low, &prev);

    // TEMP diagnostic (2026-09-14): 7 straight live sessions on MESZ6 fired
    // zero signals despite the backtest predicting ~9% of periods should
    // qualify. Logging raw features + their standardized (training-mean-
    // relative) form side by side to find which input, if any, is staying
    // artificially flat across periods live — confidence alone doesn't show
    // that. Now per-symbol (2026-09-16) since MNQZ6/NQZ6 need the same
    // visibility once they come online. Remove once the mystery's resolved.
    let raw = feature_vector(&input);
    let names = [
        "period_idx", "prev_bullish", "prev_range", "prev_vol_log", "dist_poc",
        "dist_vah", "dist_val", "bull_frac", "ib_width", "prev_delta_ratio", "cum_delta_ratio",
        "dist_rally_high", "dist_pullback_low", "dist_prior_high", "dist_prior_low",
        "prior_va_width", "prior_poc_position",
    ];
    let feat_dump: String = names
        .iter()
        .zip(raw.iter())
        .enumerate()
        .map(|(i, (name, v))| {
            let z = (v - state.trained.means[i]) / state.trained.stds[i];
            format!("{name}={v:.5}(z={z:+.2})")
        })
        .collect::<Vec<_>>()
        .join(" ");
    tracing::debug!("{symbol} period {next_period_idx} features: {feat_dump}");

    let probs: Probabilities = predict(&state.trained.models, &state.trained.means, &state.trained.stds, &input);
    tracing::debug!(
        "{symbol} period {next_period_idx} probs: bullish={:.3} break_vah={:.3} break_val={:.3} return_poc={:.3}",
        probs.p_bullish, probs.p_break_vah, probs.p_break_val, probs.p_return_poc
    );

    let r_long = risk_params(Direction::Long, &probs, 1, &state.trained.quality, risk_cfg, 1.25);
    let r_short = risk_params(Direction::Short, &probs, 1, &state.trained.quality, risk_cfg, 1.25);
    let (direction, lean_risk) =
        if r_long.confidence >= r_short.confidence { (Direction::Long, r_long.clone()) } else { (Direction::Short, r_short.clone()) };

    let result = trade_setup(direction, prev.close, &prior_levels, &lean_risk, None, setup_cfg);

    // Publish the live MODEL/RISK/TRADE SETUP read regardless of outcome —
    // bilore-cockpit displays exactly this, whether or not it locks, same
    // scope as the debug log above. See bilore_ml_rs::live_state's own
    // doc comment for why this is a flat DTO, not the internal types.
    if let Some(nc) = nats {
        let state_msg = LiveModelState {
            symbol: symbol.to_string(),
            period_idx: next_period_idx,
            ts: Utc::now(),
            p_bullish: probs.p_bullish,
            p_break_vah: probs.p_break_vah,
            p_break_val: probs.p_break_val,
            p_return_poc: probs.p_return_poc,
            long_confidence: r_long.confidence,
            long_tier: r_long.tier_label.clone(),
            long_action: format!("{:?}", r_long.action),
            long_allowed: r_long.allowed,
            long_max_loss_ticks: r_long.max_loss_ticks,
            long_max_loss_dollars: r_long.max_loss_dollars,
            short_confidence: r_short.confidence,
            short_tier: r_short.tier_label.clone(),
            short_action: format!("{:?}", r_short.action),
            short_allowed: r_short.allowed,
            short_max_loss_ticks: r_short.max_loss_ticks,
            short_max_loss_dollars: r_short.max_loss_dollars,
            lean: format!("{direction:?}"),
            strategy: strategy::ML_MODEL.to_string(),
            // Entry trust only exists alongside an actual setup (contracts/
            // strategies.md, "Trust metric"): ml-model's is the leaned
            // side's risk_params confidence.
            entry_trust: match &result {
                SetupResult::Setup(_) => Some(lean_risk.confidence),
                SetupResult::NoSetup(_) => None,
            },
            setup: match &result {
                SetupResult::Setup(s) => Some(LiveSetup {
                    direction: format!("{:?}", s.direction),
                    entry: s.entry_price.to_f64().unwrap_or(0.0),
                    stop: s.stop.to_f64().unwrap_or(0.0),
                    target: s.targets[0].price.to_f64().unwrap_or(0.0),
                }),
                SetupResult::NoSetup(_) => None,
            },
            locked: false, // set true below if this one actually gets locked
            reason: match &result {
                SetupResult::NoSetup(reason) => Some(reason.clone()),
                SetupResult::Setup(_) => None,
            },
        };
        if let Ok(payload) = serde_json::to_vec(&state_msg) {
            let _ = nc.publish(bilore_ml_rs::live_state::subject(symbol), payload.into()).await;
        }
    }

    let setup = match result {
        SetupResult::Setup(s) => s,
        SetupResult::NoSetup(reason) => {
            tracing::debug!("{symbol}: no ml-model setup for period {next_period_idx}: {reason}");
            return;
        }
    };

    tracing::info!(
        "{symbol} LOCKED [ml-model]: {:?} entry={} stop={} target={} confidence={:.2} tier={}",
        setup.direction, setup.entry_price, setup.stop, setup.targets[0].price, lean_risk.confidence, lean_risk.tier_label
    );

    let mut trade = ShadowTrade::new(direction, setup.entry_price, setup.entry_price, setup.stop, Some(setup.targets[0].price), 1, true);
    trade.outcome = Outcome::Entered;
    trade.entry_price = Some(setup.entry_price);

    match db::insert_shadow_trade_v2(pool, state.today_ct, symbol, strategy::ML_MODEL, Some(lean_risk.confidence), &trade).await {
        Ok(id) => state.ml.open = Some(OpenTrade { db_id: id, trade, trust: lean_risk.confidence }),
        Err(e) => tracing::error!("{symbol}: insert_shadow_trade_v2 failed: {e}"),
    }
    state.ml.locked_today = true;

    if let Some(nc) = nats {
        let state_msg = LiveModelState {
            symbol: symbol.to_string(),
            period_idx: next_period_idx,
            ts: Utc::now(),
            p_bullish: probs.p_bullish,
            p_break_vah: probs.p_break_vah,
            p_break_val: probs.p_break_val,
            p_return_poc: probs.p_return_poc,
            long_confidence: r_long.confidence,
            long_tier: r_long.tier_label.clone(),
            long_action: format!("{:?}", r_long.action),
            long_allowed: r_long.allowed,
            long_max_loss_ticks: r_long.max_loss_ticks,
            long_max_loss_dollars: r_long.max_loss_dollars,
            short_confidence: r_short.confidence,
            short_tier: r_short.tier_label.clone(),
            short_action: format!("{:?}", r_short.action),
            short_allowed: r_short.allowed,
            short_max_loss_ticks: r_short.max_loss_ticks,
            short_max_loss_dollars: r_short.max_loss_dollars,
            lean: format!("{direction:?}"),
            strategy: strategy::ML_MODEL.to_string(),
            entry_trust: Some(lean_risk.confidence),
            setup: Some(LiveSetup {
                direction: format!("{:?}", setup.direction),
                entry: setup.entry_price.to_f64().unwrap_or(0.0),
                stop: setup.stop.to_f64().unwrap_or(0.0),
                target: setup.targets[0].price.to_f64().unwrap_or(0.0),
            }),
            locked: true,
            reason: None,
        };
        if let Ok(payload) = serde_json::to_vec(&state_msg) {
            let _ = nc.publish(bilore_ml_rs::live_state::subject(symbol), payload.into()).await;
        }
    }

    sound::play(sound::AlertKind::Setup);
    let msg = format!(
        "🟢 <b>[V2][ml-model] SETUP — {symbol}</b>\nDirection : <b>{:?}</b>\nEntry     : <b>{:.2}</b>\nStop      : {:.2}\nTarget    : {:.2}\nTrust     : {:.0}% ({})",
        setup.direction, setup.entry_price, setup.stop, setup.targets[0].price, lean_risk.confidence * 100.0, lean_risk.tier_label
    );
    notify(http, bot_token, chat_id, &msg).await;
}

/// `fade-poc` strategy (2026-09-18) — same machinery as `try_signal`, with
/// direction overridden by the fade-toward-POC rule (`bilore_monitor::fade`,
/// `bilore-project-conf/contracts/strategies.md`). Mirrors the validated
/// `backtest_tick_sim_direction.rs` fade arm: the trained model's
/// probabilities still feed `risk_params` sizing — only direction selection
/// differs. Tracks its own shadow trade in the `state.fade` slot, fully
/// independent of the `ml-model` slot.
#[allow(clippy::too_many_arguments)]
async fn try_fade_signal(
    pool: &PgPool,
    http: &reqwest::Client,
    bot_token: &str,
    chat_id: &str,
    nats: Option<&async_nats::Client>,
    symbol: &str,
    risk_cfg: &RiskConfig,
    setup_cfg: &TradeSetupConfig,
    state: &mut PerSymbolState,
) {
    let Some(prev) = state.last_completed else { return };
    let next_period_idx = prev.period_idx + 1;
    if next_period_idx > 12 {
        return; // past the 13-period RTH cap (A-M)
    }

    let Some(prior_levels) = state.prior else { return };

    let metrics = state.tpo.metrics_for_date(state.today_ct);
    let (ib_high, ib_low) = match &metrics {
        Some(m) => (m.ib_high, m.ib_low),
        None => return, // no trades recorded yet today — nothing to score against
    };

    let input = build_live_input(state, &prior_levels, ib_high, ib_low, &prev);
    let probs: Probabilities = predict(&state.trained.models, &state.trained.means, &state.trained.stds, &input);
    let r_long = risk_params(Direction::Long, &probs, 1, &state.trained.quality, risk_cfg, 1.25);
    let r_short = risk_params(Direction::Short, &probs, 1, &state.trained.quality, risk_cfg, 1.25);

    // The one difference from try_signal: direction comes from the fade
    // rule (price vs prior POC), not from which side has higher confidence.
    let direction = fade::fade_direction(prev.close, prior_levels.poc);
    let lean_risk = if direction == Direction::Long { r_long.clone() } else { r_short.clone() };

    let result = trade_setup(direction, prev.close, &prior_levels, &lean_risk, None, setup_cfg);

    // Publish the live state (strategy-tagged) regardless of outcome, same
    // contract as try_signal — the cockpit displays this.
    if let Some(nc) = nats {
        let state_msg = LiveModelState {
            symbol: symbol.to_string(),
            period_idx: next_period_idx,
            ts: Utc::now(),
            p_bullish: probs.p_bullish,
            p_break_vah: probs.p_break_vah,
            p_break_val: probs.p_break_val,
            p_return_poc: probs.p_return_poc,
            long_confidence: r_long.confidence,
            long_tier: r_long.tier_label.clone(),
            long_action: format!("{:?}", r_long.action),
            long_allowed: r_long.allowed,
            long_max_loss_ticks: r_long.max_loss_ticks,
            long_max_loss_dollars: r_long.max_loss_dollars,
            short_confidence: r_short.confidence,
            short_tier: r_short.tier_label.clone(),
            short_action: format!("{:?}", r_short.action),
            short_allowed: r_short.allowed,
            short_max_loss_ticks: r_short.max_loss_ticks,
            short_max_loss_dollars: r_short.max_loss_dollars,
            lean: format!("{direction:?}"),
            strategy: strategy::FADE_POC.to_string(),
            // Entry trust only exists alongside an actual setup
            // (contracts/strategies.md, "Trust metric"): fade-poc's is the
            // POC-extension score — 50% at the prior POC, 100% a full stop
            // beyond it.
            entry_trust: match &result {
                SetupResult::Setup(s) =>
                    Some(fade::fade_trust(prev.close, prior_levels.poc, (s.stop - s.entry_price).abs())),
                SetupResult::NoSetup(_) => None,
            },
            setup: match &result {
                SetupResult::Setup(s) => Some(LiveSetup {
                    direction: format!("{:?}", s.direction),
                    entry: s.entry_price.to_f64().unwrap_or(0.0),
                    stop: s.stop.to_f64().unwrap_or(0.0),
                    target: s.targets[0].price.to_f64().unwrap_or(0.0),
                }),
                SetupResult::NoSetup(_) => None,
            },
            locked: false, // set true below if this one actually gets locked
            reason: match &result {
                SetupResult::NoSetup(reason) => Some(reason.clone()),
                SetupResult::Setup(_) => None,
            },
        };
        if let Ok(payload) = serde_json::to_vec(&state_msg) {
            let _ = nc.publish(bilore_ml_rs::live_state::subject(symbol), payload.into()).await;
        }
    }

    let setup = match result {
        SetupResult::Setup(s) => s,
        SetupResult::NoSetup(reason) => {
            tracing::debug!("{symbol}: no fade-poc setup for period {next_period_idx}: {reason}");
            return;
        }
    };

    tracing::info!(
        "{symbol} LOCKED [fade-poc]: {:?} entry={} stop={} target={} (prior POC={}, close {})",
        setup.direction, setup.entry_price, setup.stop, setup.targets[0].price,
        prior_levels.poc, if prev.close > prior_levels.poc { "above" } else { "at/below" }
    );

    // Entry trust (contracts/strategies.md, "Trust metric"): how far price
    // extended beyond the prior POC relative to the stop distance.
    let trust = fade::fade_trust(prev.close, prior_levels.poc, (setup.stop - setup.entry_price).abs());

    let mut trade = ShadowTrade::new(direction, setup.entry_price, setup.entry_price, setup.stop, Some(setup.targets[0].price), 1, true);
    trade.outcome = Outcome::Entered;
    trade.entry_price = Some(setup.entry_price);

    match db::insert_shadow_trade_v2(pool, state.today_ct, symbol, strategy::FADE_POC, Some(trust), &trade).await {
        Ok(id) => state.fade.open = Some(OpenTrade { db_id: id, trade, trust }),
        Err(e) => tracing::error!("{symbol}: insert_shadow_trade_v2 (fade-poc) failed: {e}"),
    }
    state.fade.locked_today = true;

    if let Some(nc) = nats {
        let state_msg = LiveModelState {
            symbol: symbol.to_string(),
            period_idx: next_period_idx,
            ts: Utc::now(),
            p_bullish: probs.p_bullish,
            p_break_vah: probs.p_break_vah,
            p_break_val: probs.p_break_val,
            p_return_poc: probs.p_return_poc,
            long_confidence: r_long.confidence,
            long_tier: r_long.tier_label.clone(),
            long_action: format!("{:?}", r_long.action),
            long_allowed: r_long.allowed,
            long_max_loss_ticks: r_long.max_loss_ticks,
            long_max_loss_dollars: r_long.max_loss_dollars,
            short_confidence: r_short.confidence,
            short_tier: r_short.tier_label.clone(),
            short_action: format!("{:?}", r_short.action),
            short_allowed: r_short.allowed,
            short_max_loss_ticks: r_short.max_loss_ticks,
            short_max_loss_dollars: r_short.max_loss_dollars,
            lean: format!("{direction:?}"),
            strategy: strategy::FADE_POC.to_string(),
            entry_trust: Some(trust),
            setup: Some(LiveSetup {
                direction: format!("{:?}", setup.direction),
                entry: setup.entry_price.to_f64().unwrap_or(0.0),
                stop: setup.stop.to_f64().unwrap_or(0.0),
                target: setup.targets[0].price.to_f64().unwrap_or(0.0),
            }),
            locked: true,
            reason: None,
        };
        if let Ok(payload) = serde_json::to_vec(&state_msg) {
            let _ = nc.publish(bilore_ml_rs::live_state::subject(symbol), payload.into()).await;
        }
    }

    sound::play(sound::AlertKind::Setup);
    let msg = format!(
        "🟢 <b>[V2][fade-poc] SETUP — {symbol}</b>\nDirection : <b>{:?}</b>\nEntry     : <b>{:.2}</b>\nStop      : {:.2}\nTarget    : {:.2}\nPrior POC : {:.2} (close {})\nTrust     : {:.0}%",
        setup.direction, setup.entry_price, setup.stop, setup.targets[0].price, prior_levels.poc,
        if prev.close > prior_levels.poc { "above" } else { "at/below" }, trust * 100.0
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
