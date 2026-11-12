# exec

Source: `src/exec.rs` (pure) · wired in `main.rs` (`maybe_execute`, called after each shadow-trade
insert in `try_priorpoc_signal`, `try_pf_signal`, `try_globex_signal`, `try_roll_signal`) ·
New 2026-10-18 (MON-021, bilore-specs SYS-009-R1) · per-strategy mode 2026-10-26 (MON-023, SYS-014).

After a shadow trade is recorded, `maybe_execute`:
1. re-reads `bilore-ml/config/trading_rules.toml` (`BILORE_TRADING_RULES` overrides the path), so
   a config edit needs no restart. The strategy's mode is `ExecConfig::mode_for(slug)`: `live` if
   `[execution] live` lists it, `paper` if `paper` does; in neither: nothing happens (silent). The
   old `mode` / `strategies` keys or a strategy in both lists: a config error, logged, nothing
   executes;
2. reads today's realized dollars per strategy from `live_orders` in that strategy's mode (its
   account), the go-live gate (live only), the kill-switch file `~/.config/bilore/kill_switch`, the
   mode's open orders (`db::open_orders`: placing, working or filled, for SYS-016's cap), and
   the saved caps from `strategy_caps` (SYS-001-R2; unreadable: nothing executes). A strategy that
   just reached its cap is saved there (closing an earlier day's cap row still open) and reported on
   Telegram; it is back by itself on the next Chicago day (amended 2026-11-12), or with
   `bilore enable <strategy>`. A `manual` row (`bilore disable <strategy>`) holds until
   `bilore enable`, without a restart;
3. asks `bilore_core::execution::may_execute`. Allowed: publishes `OrderIntent::new_order` on
   `intent_subject(mode)` = `bilore.order.intent.paper` / `.live` through JetStream (stream
   `BILORE_ORDERS`, created if missing) and logs `INTENT [slug] #id <mode>: …`. Refused: logs
   `stays shadow-only (<mode>): <reason>`.

Follow-ups (cancel/close) aren't sent: the executor follows the shadow row's outcome in the DB.

| Symbol | Purpose | Tests |
|---|---|---|
| `read_config()` | `[execution]` `paper`, `live`, `autotrade` (v2's live switch; v1's `[platform.ib_tws] autotrade_enabled` is not read) and the account cap; `max_open_live` / `max_open_paper` (SYS-016; absent = no limit, < 1 refused); `[daily_loss_cap]` | `the_open_order_cap_is_read_per_mode`, `execution_config_is_read_from_trading_rules`, `missing_execution_section_means_off`, `the_old_single_mode_and_a_strategy_in_both_lists_are_refused` |
| `checks()` | The gate inputs; SYS-001 book with live semantics in both modes | `checks_use_todays_realized_dollars_and_the_caps` |
| `SavedCap` / `book_from()` / `newly_capped()` | An open strategy_caps row (cap or manual disable); the cap book from them; whether this check just capped the strategy (not yet saved for that day) | `a_saved_cap_survives_a_restart`, `only_a_cap_not_already_saved_is_new` |
| `kill_switch_path()` | `~/.config/bilore/kill_switch` | — |

Known limits: a failed publish leaves that trade shadow-only (logged as an error).
