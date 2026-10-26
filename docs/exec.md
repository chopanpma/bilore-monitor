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
   account), the go-live gate (live only), and the kill-switch file `~/.config/bilore/kill_switch`;
3. asks `bilore_core::execution::may_execute`. Allowed: publishes `OrderIntent::new_order` on
   `intent_subject(mode)` = `bilore.order.intent.paper` / `.live` through JetStream (stream
   `BILORE_ORDERS`, created if missing) and logs `INTENT [slug] #id <mode>: …`. Refused: logs
   `stays shadow-only (<mode>): <reason>`.

Follow-ups (cancel/close) aren't sent: the executor follows the shadow row's outcome in the DB.

| Symbol | Purpose | Tests |
|---|---|---|
| `read_config()` | `[execution]` `paper`, `live`, `autotrade` (v2's live switch; v1's `[platform.ib_tws] autotrade_enabled` is not read) and the account cap; `[daily_loss_cap]` | `execution_config_is_read_from_trading_rules`, `missing_execution_section_means_off`, `the_old_single_mode_and_a_strategy_in_both_lists_are_refused` |
| `checks()` | The gate inputs; SYS-001 book with live semantics in both modes | `checks_use_todays_realized_dollars_and_the_caps` |
| `kill_switch_path()` | `~/.config/bilore/kill_switch` | — |

Known limits: the per-strategy cap book is in memory (a restart re-enables a capped strategy);
a failed publish leaves that trade shadow-only (logged as an error).
