//! Telegram notifications for the v2 parallel monitor — same bot/chat as
//! v1's `telegram_notify.py` (reuses `TELEGRAM_BOT_TOKEN`/`TELEGRAM_CHAT_ID`
//! from `bilore-ml/.env`), every message prefixed `[V2]` so the two
//! streams are tellable apart in one chat without a second bot. Message
//! builders are pure (tested); the actual HTTP call is a thin, untested
//! I/O wrapper, same convention as `bilore-cockpit::db`.

use bilore_backtest::signal::GeneratedSignal;
use bilore_core::market_structure::Direction;
use bilore_core::shadow_trader::{Outcome, ShadowTrade};
use rust_decimal::Decimal;

fn direction_str(d: Direction) -> &'static str {
    match d {
        Direction::Long => "LONG",
        Direction::Short => "SHORT",
    }
}

pub fn setup_message(symbol: &str, sig: &GeneratedSignal) -> String {
    format!(
        "🟢 <b>[V2] SETUP — {symbol}</b>\n\
         Direction : <b>{}</b>\n\
         Entry     : <b>{:.2}</b>\n\
         Stop      : {:.2}\n\
         Target    : {:.2}\n\
         Confirmed : {}",
        direction_str(sig.direction),
        sig.entry_price,
        sig.stop_price,
        sig.target_price,
        if sig.would_confirm { "✅ yes (structure + order flow agree)" } else { "⚠️ no (structure only)" },
    )
}

fn outcome_icon(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Won => "✅",
        Outcome::Lost => "❌",
        Outcome::Expired => "⏱️",
        _ => "•",
    }
}

/// `trust` is the strategy's entry-trust score (0..1) captured when the
/// trade was locked — same value persisted to `shadow_trades_v2.confidence`
/// (see contracts/strategies.md, "Trust metric").
pub fn result_message(symbol: &str, strategy: &str, trade: &ShadowTrade, trust: f64) -> String {
    let pnl = match (trade.pnl_ticks, trade.pnl_dollars) {
        (Some(t), Some(d)) => format!("{t:+.1}t  ${d:+.2}"),
        _ => "—".to_string(),
    };
    format!(
        "{} <b>[V2][{}] {} — {symbol}</b>\n\
         Direction : {}\n\
         Entry     : {}\n\
         Exit      : {}\n\
         Trust     : {:.0}%\n\
         P&L       : {pnl}",
        outcome_icon(trade.outcome),
        strategy,
        trade.outcome.as_str().to_uppercase(),
        direction_str(trade.direction),
        fmt_opt(trade.entry_price),
        fmt_opt(trade.exit_price),
        trust * 100.0,
    )
}

fn fmt_opt(v: Option<Decimal>) -> String {
    v.map(|d| format!("{d:.2}")).unwrap_or_else(|| "—".to_string())
}

pub async fn send(client: &reqwest::Client, token: &str, chat_id: &str, text: &str) -> anyhow::Result<()> {
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    client
        .post(&url)
        .json(&serde_json::json!({ "chat_id": chat_id, "text": text, "parse_mode": "HTML" }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilore_core::shadow_trader::Outcome;
    use chrono::{TimeZone, Utc};

    fn signal(direction: Direction, would_confirm: bool) -> GeneratedSignal {
        GeneratedSignal {
            direction,
            entry_price: "5000.00".parse().unwrap(),
            entry_ts: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            stop_price: "4995.00".parse().unwrap(),
            target_price: "5010.00".parse().unwrap(),
            would_confirm,
        }
    }

    #[test]
    fn setup_message_includes_symbol_direction_and_prices() {
        let msg = setup_message("MESU6", &signal(Direction::Long, true));
        assert!(msg.contains("[V2] SETUP — MESU6"));
        assert!(msg.contains("LONG"));
        assert!(msg.contains("5000.00"));
        assert!(msg.contains("4995.00"));
        assert!(msg.contains("5010.00"));
    }

    #[test]
    fn setup_message_shows_confirmed_state() {
        assert!(setup_message("MESU6", &signal(Direction::Long, true)).contains('✅'));
        assert!(setup_message("MESU6", &signal(Direction::Long, false)).contains("⚠️"));
    }

    fn trade(outcome: Outcome, entry: &str, exit: &str, pnl_ticks: &str, pnl_dollars: &str) -> ShadowTrade {
        let mut t = ShadowTrade::new(
            Direction::Long,
            entry.parse().unwrap(),
            entry.parse().unwrap(),
            "4995".parse().unwrap(),
            Some("5010".parse().unwrap()),
            1,
            true,
        );
        t.outcome = outcome;
        t.entry_price = Some(entry.parse().unwrap());
        t.exit_price = Some(exit.parse().unwrap());
        t.pnl_ticks = Some(pnl_ticks.parse().unwrap());
        t.pnl_dollars = Some(pnl_dollars.parse().unwrap());
        t
    }

    #[test]
    fn result_message_shows_won_outcome_and_pnl() {
        let msg = result_message("MESU6", "ml-model", &trade(Outcome::Won, "5000", "5010", "40", "50.00"), 0.62);
        assert!(msg.contains("[V2][ml-model] WON — MESU6"));
        assert!(msg.contains("+40.0t"));
        assert!(msg.contains("$+50.00"));
        assert!(msg.contains("Trust     : 62%"));
        assert!(msg.contains('✅'));
    }

    #[test]
    fn result_message_shows_lost_outcome() {
        let msg = result_message("MESU6", "fade-poc", &trade(Outcome::Lost, "5000", "4995", "-20", "-25.00"), 0.75);
        assert!(msg.contains("[V2][fade-poc] LOST — MESU6"));
        assert!(msg.contains("Trust     : 75%"));
        assert!(msg.contains('❌'));
    }
}
