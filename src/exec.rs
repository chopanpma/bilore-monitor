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
    let caps: Vec<(String, Decimal)> = v
        .get("daily_loss_cap")
        .and_then(|t| t.as_table())
        .map(|t| t.iter().filter_map(|(root, cap)| num(cap).map(|c| (root.clone(), c))).collect())
        .unwrap_or_default();
    let caps = LossCapConfig::new(caps.iter().map(|(r, c)| (r.as_str(), *c)));
    let e = v.get("execution");
    // SYS-014: one mode per strategy. The old single `mode` + `strategies`
    // is refused, so nothing executes until the config is migrated.
    if e.is_some_and(|e| e.get("mode").is_some() || e.get("strategies").is_some()) {
        return Err("[execution] mode/strategies were replaced by per-strategy paper = [...] and live = [...] lists (SYS-014)".into());
    }
    let list = |key: &str| -> Vec<String> {
        e.and_then(|e| e.get(key))
            .and_then(|s| s.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    // v2's own live switch; v1's [platform.ib_tws] autotrade_enabled drives v1 only.
    let autotrade_enabled = e.and_then(|e| e.get("autotrade")).and_then(|a| a.as_bool()).unwrap_or(false);
    let account_cap = e.and_then(|e| e.get("account_daily_loss_cap_usd")).and_then(num);
    let exec = ExecConfig { paper: list("paper"), live: list("live"), account_cap };
    exec.validate()?;
    Ok(ExecSettings { exec, autotrade_enabled, caps })
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
autotrade_enabled  = true   # v1's switch: v2 must not read it (SYS-014)

[daily_loss_cap]
MES = 100.0

[execution]
autotrade = false
paper = ["fade-priorpoc-va60-1r", "fade-priorpoc-va80-1.5r"]
live  = ["fade-roll-overnight"]
account_daily_loss_cap_usd = 100
"#;

    #[test]
    fn execution_config_is_read_from_trading_rules() {
        let s = read_config(RULES).unwrap();
        assert_eq!(s.exec.paper, vec!["fade-priorpoc-va60-1r".to_string(), "fade-priorpoc-va80-1.5r".to_string()]);
        assert_eq!(s.exec.live, vec!["fade-roll-overnight".to_string()]);
        assert_eq!(s.exec.mode_for("fade-roll-overnight"), ExecMode::Live);
        assert_eq!(s.exec.account_cap, Some(d("100")));
        assert!(!s.autotrade_enabled, "[execution] autotrade, not v1's [platform.ib_tws] autotrade_enabled");
        assert_eq!(s.caps.cap_for("MESZ6"), Some(d("100")));
        let on = read_config(&RULES.replace("autotrade = false", "autotrade = true")).unwrap();
        assert!(on.autotrade_enabled);
    }

    #[test]
    fn missing_execution_section_means_off() {
        let s = read_config("[platform.ib_tws]\nautotrade_enabled = true\n").unwrap();
        assert_eq!((s.exec.paper.len(), s.exec.live.len(), s.exec.account_cap, s.autotrade_enabled), (0, 0, None, false));
    }

    #[test]
    fn the_old_single_mode_and_a_strategy_in_both_lists_are_refused() {
        let old = "[execution]\nmode = \"paper\"\nstrategies = [\"fade-priorpoc-2r\"]\n";
        assert!(read_config(old).unwrap_err().contains("SYS-014"));
        let both = "[execution]\npaper = [\"fade-roll-overnight\"]\nlive = [\"fade-roll-overnight\"]\n";
        assert!(read_config(both).unwrap_err().contains("both paper and live"));
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
