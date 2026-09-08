# bilore-monitor module index

Same convention as every other v2 crate: one diagram doc per logic file
under `src/`, enforced by a pre-commit hook (`.githooks/pre-commit`, wired
via `git config core.hooksPath .githooks`). `db.rs` (DB I/O) and `main.rs`
(CLI wiring) are exempt — no decision logic to diagram.

| File | Status | Diagram |
|---|---|---|
| `live_signal.rs` | done | [live_signal.md](live_signal.md) |
| `telegram.rs` | done | [telegram.md](telegram.md) |
| `db.rs` | exempt (I/O) | — |
| `main.rs` | exempt (wiring) | — |

## What this crate is

Headless parallel monitor, built 2026-09-08 at the user's request after
tracing "no setup in the cockpit's confirmation zone" to `daily_plans`
being silently orphaned since 2026-06-18 (`bilore_session.py` — the script
actually run day to day — generates setups and shadow-trades them in its
own process, but never got the `write_daily_plan` call `rth_monitor.py`
has). Rather than patch v1, this crate generates its OWN live setups from
`bilore-core`'s structure + order-flow confirmation (`bilore-backtest`'s
already-validated logic, reused via `live_signal.rs`'s streaming wrapper),
tracks one shadow trade per session in `shadow_trades_v2`, and sends
`[V2]`-prefixed Telegram alerts on the same bot/chat as v1 — so the two
approaches can be compared live, with an eye toward eventually
standardizing the cockpit on whichever performs better.

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
- Entry/stop/target logic mirrors `bilore-backtest`'s fresh-signal
  generator exactly (swing pivot + tick buffer stop, fixed R-multiple
  target) — the same caveat applies: this is a rule invented for testing
  the confirmation-gate hypothesis, not a port of v1's real
  `trade_setup()`/`risk_params()` ML-based sizing.
- One shadow trade tracked per session — a second `NewSignal` while one is
  still open is ignored, matching v1's one-setup-per-day semantics.
