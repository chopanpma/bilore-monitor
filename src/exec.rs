//! Order intents from shadow trades (MON-021, bilore-specs SYS-009-R1) —
//! pure parts; `main.rs` does the I/O (re-reading the trading rules, the
//! kill-switch file, live_orders, publishing on JetStream).

use bilore_core::execution::{Checks, ExecConfig};
use bilore_core::loss_cap::{LossCapBook, LossCapConfig};
use chrono::NaiveDate;
use rust_decimal::Decimal;

/// Where the cockpit's kill switch lives (`~/.config/bilore/kill_switch`).
pub fn kill_switch_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join(".config/bilore/kill_switch")
}

/// Everything the gate needs from bilore-ml/config/trading_rules.toml.
#[derive(Debug, Clone)]
pub struct ExecSettings {
    pub exec: ExecConfig,
    pub autotrade_enabled: bool,
    pub caps: LossCapConfig,
}

pub fn read_config(toml_text: &str) -> Result<ExecSettings, String> {
    let v: toml::Value = toml_text.parse().map_err(|e| format!("trading_rules.toml: {e}"))?;
    let num = |x: &toml::Value| -> Option<Decimal> {
        match x {
            toml::Value::Integer(i) => Some(Decimal::from(*i)),
            toml::Value::Float(f) => Decimal::try_from(*f).ok(),
            _ => None,
        }
    };
    let autotrade_enabled = v
        .get("platform")
        .and_then(|p| p.get("ib_tws"))
        .and_then(|t| t.get("autotrade_enabled"))
        .and_then(|a| a.as_bool())
        .unwrap_or(false);
    let caps: Vec<(String, Decimal)> = v
        .get("daily_loss_cap")
        .and_then(|t| t.as_table())
        .map(|t| t.iter().filter_map(|(root, cap)| num(cap).map(|c| (root.clone(), c))).collect())
        .unwrap_or_default();
    let caps = LossCapConfig::new(caps.iter().map(|(r, c)| (r.as_str(), *c)));
    let e = v.get("execution");
    let mode = match e.and_then(|e| e.get("mode")).and_then(|m| m.as_str()).unwrap_or("off") {
        "paper" => bilore_core::execution::ExecMode::Paper,
        "live" => bilore_core::execution::ExecMode::Live,
        "off" => bilore_core::execution::ExecMode::Off,
        other => return Err(format!("[execution] mode must be off, paper or live, not {other:?}")),
    };
    let strategies = e
        .and_then(|e| e.get("strategies"))
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let account_cap = e.and_then(|e| e.get("account_daily_loss_cap_usd")).and_then(num);
    Ok(ExecSettings { exec: ExecConfig { mode, strategies, account_cap }, autotrade_enabled, caps })
}

/// The gate's inputs for `slug` right now. `realized` is today's realized
/// dollars per strategy in the configured mode (from live_orders).
#[allow(clippy::too_many_arguments)]
pub fn checks(
    settings: &ExecSettings,
    slug: &str,
    symbol: &str,
    realized: &[(String, Decimal)],
    book: &mut LossCapBook,
    today: NaiveDate,
    gate_passed: bool,
    volatility_block: bool,
    kill_switch: bool,
) -> Checks {
    let mine = realized.iter().filter(|(s, _)| s == slug).map(|(_, d)| *d).sum::<Decimal>();
    let account = realized.iter().map(|(_, d)| *d).sum::<Decimal>();
    // SYS-001 with live semantics in both modes (paper measures live).
    let strategy_cap = book.check_entry(slug, bilore_core::loss_cap::Mode::Live, symbol, mine, today, &settings.caps);
    Checks {
        autotrade_enabled: settings.autotrade_enabled,
        gate_passed,
        strategy_cap,
        account_realized_today: account,
        volatility_block,
        kill_switch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bilore_core::execution::ExecMode;
    use bilore_core::loss_cap::{Entry, Refusal};

    fn d(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    const RULES: &str = r#"
[platform.ib_tws]
autotrade_enabled  = false

[daily_loss_cap]
MES = 100.0

[execution]
mode       = "paper"
strategies = ["fade-priorpoc-va60-1r", "fade-priorpoc-va80-1.5r"]
account_daily_loss_cap_usd = 100
"#;

    #[test]
    fn execution_config_is_read_from_trading_rules() {
        let s = read_config(RULES).unwrap();
        assert_eq!(s.exec.mode, ExecMode::Paper);
        assert_eq!(s.exec.strategies, vec!["fade-priorpoc-va60-1r".to_string(), "fade-priorpoc-va80-1.5r".to_string()]);
        assert_eq!(s.exec.account_cap, Some(d("100")));
        assert!(!s.autotrade_enabled);
        assert_eq!(s.caps.cap_for("MESZ6"), Some(d("100")));
    }

    #[test]
    fn missing_execution_section_means_off() {
        let s = read_config("[platform.ib_tws]\nautotrade_enabled = false\n").unwrap();
        assert_eq!((s.exec.mode, s.exec.strategies.len(), s.exec.account_cap), (ExecMode::Off, 0, None));
    }

    #[test]
    fn checks_use_todays_realized_dollars_and_the_caps() {
        let s = read_config(RULES).unwrap();
        let realized = vec![("fade-priorpoc-va60-1r".to_string(), d("-100")), ("fade-priorpoc-va80-1.5r".to_string(), d("-20"))];
        let today = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
        let mut book = LossCapBook::default();
        let va60 = checks(&s, "fade-priorpoc-va60-1r", "MESZ6", &realized, &mut book, today, false, false, false);
        assert_eq!(va60.strategy_cap, Entry::Refused(Refusal::DailyCapReached { cap: d("100") }));
        assert_eq!(va60.account_realized_today, d("-120"));
        let va80 = checks(&s, "fade-priorpoc-va80-1.5r", "MESZ6", &realized, &mut book, today, false, true, true);
        assert_eq!(va80.strategy_cap, Entry::Allowed);
        assert_eq!((va80.account_realized_today, va80.volatility_block, va80.kill_switch), (d("-120"), true, true));
        assert!(!va80.autotrade_enabled && !va80.gate_passed);
    }
}
