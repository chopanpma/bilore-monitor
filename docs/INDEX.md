# bilore-monitor module index

Same convention as every other v2 crate: one diagram doc per logic file
under `src/`, enforced by a pre-commit hook (`.githooks/pre-commit`, wired
via `git config core.hooksPath .githooks`). `db.rs` (DB I/O) and `main.rs`
(CLI wiring) are exempt — no decision logic to diagram.

| File | Status | Diagram |
|---|---|---|
| `period_agg.rs` | done | [period_agg.md](period_agg.md) |
| `live_signal.rs` | done, superseded (see below) | [live_signal.md](live_signal.md) |
| `telegram.rs` | done | [telegram.md](telegram.md) |
| `sound.rs` | done | [sound.md](sound.md) |
| `db.rs` | exempt (I/O) | — |
| `main.rs` | exempt (wiring) | — |

## What this crate is

Headless parallel monitor, built 2026-09-08 at the user's request after
tracing "no setup in the cockpit's confirmation zone" to `daily_plans`
being silently orphaned since 2026-06-18 (`bilore_session.py` — the script
actually run day to day — generates setups and shadow-trades them in its
own process, but never got the `write_daily_plan` call `rth_monitor.py`
has). Tracks one shadow trade per session in `shadow_trades_v2`, sends
`[V2]`-prefixed Telegram alerts on the same bot/chat as v1.

**Signal source, revised same day.** First version (`live_signal.rs` +
`bilore-backtest::signal`) used structure + order-flow confirmation alone.
The user then asked to replicate v1's REAL logic instead — `predict()` (4
logistic-regression classifiers) -> `risk_params()` -> `trade_setup()`,
faithfully ported to `bilore-ml-rs` (`risk.rs`, `trade_setup.rs`,
`predict.rs`, `features.rs`, `db.rs`, all with real test coverage). Two
backtests of that full port came back suspiciously strong (76.3%, then
74.1% after two real look-ahead fixes — still well above v1's real 61.3%)
and couldn't be fully verified clean in the time available. Rather than
keep chasing the backtest, the decision was to trust v1's real, forward-only
track record and deploy the Rust port LIVE in shadow mode instead — this
binary now runs that pipeline for real, generating genuine live comparison
data no backtest bug can taint. `live_signal.rs`/`bilore-backtest::signal`
are left in place (still used by `bilore-backtest-fresh`) but no longer
drive this binary.

**What `main.rs` does now:**
- Trains fresh from `bilore-ml-rs::db::load_periods` at startup and on
  every CT day rollover (no train/test split — matches v1's own "retrain
  every run" convention; this is the live path, not evaluation).
- `period_agg::PeriodAggregator` builds live 30-min RTH period bars
  (open/high/low/close/volume/up_vol/down_vol) from ticks.
- A live `tpo_builder::TpoProfile` (RTH config) gives today's own
  VAH/VAL/IB — fed the same ticks.
- The PRIOR session's POC/VAH/VAL come from one `session_profiles` query
  per day (never today's own — that's the look-ahead bug already found and
  fixed in `bilore-ml-rs`).
- On each period boundary, if nothing's locked yet today:
  `predict()` -> `risk_params()` for both directions (higher-confidence one
  wins) -> `trade_setup()` (anchored on PRIOR-session levels only, same
  fix). A `Setup` locks a real, immediate-entry shadow trade, persisted to
  `shadow_trades_v2`, and fires a `[V2]` Telegram alert; a `NoSetup` is
  logged and the monitor keeps waiting for the next period.

Run: `SYMBOL=MESU6 DATABASE_URL=postgres://bilore:bilore@localhost:5432/bilore cargo run --bin bilore-monitor`
(auto-started by `bilore-cockpit`'s supervisor too — see its
`docs/supervisor.md`.)

## Known scope notes

- Deliberately does NOT depend on `bilore-cockpit` as a library (avoids
  pulling in ratatui for a headless service) — `db.rs` is self-contained,
  with its own `insert_shadow_trade_v2`/`update_shadow_trade_v2` that
  don't need a `DailyPlanRow` the way `bilore-cockpit::db`'s do (this
  monitor has no `daily_plans` row at all — that's the point).
- `chrono-tz` is a dependency here, unlike `bilore-replay`/`bilore-backtest`/
  `bilore-cockpit` (which deliberately avoid it, letting Postgres do
  DST-aware `AT TIME ZONE` conversions in SQL instead) — this process needs
  to know "has the CT calendar day rolled over" continuously, in memory,
  with no DB round-trip appropriate for every 1s tick — a real, justified
  exception to that convention, not an oversight.
- One shadow trade tracked per session — no re-signal once locked, matching
  v1's one-setup-per-day semantics. Unlike v1, there's no "unlock on lean
  flip" here (that concept existed in the superseded `live_signal.rs`
  version; not carried over — a simplification, not an oversight).
- The new period's "current price" (fed to `trade_setup()` as `last`) uses
  the just-completed period's CLOSE, since the new period hasn't printed a
  tick yet at the exact moment it starts — a reasonable proxy, not an exact
  match for "the live price right now."
- Entry is immediate-market-order-style (`entry_lo == entry_hi ==
  entry_price`, started directly in `Entered` state) — there's no
  daily_plans zone to wait for, matching the superseded version's same
  convention.
