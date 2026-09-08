//! Local audio alerts (macOS `afplay`) — mirrors v1's `morning_prep.py`'s
//! `_alert_zone()` (Funk/Glass for a normal zone touch, Hero/Ping when all
//! signals confirm), but with entirely different sounds so a v2 alert is
//! distinguishable from a v1 one by ear alone, not just by reading the
//! Telegram message. v1's sounds are never reused here.

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Setup,
    Won,
    Lost,
    Expired,
}

/// Primary + fallback sound for each alert kind — none of these overlap
/// with v1's set (Funk, Glass, Hero, Ping).
fn sounds_for(kind: AlertKind) -> [&'static str; 2] {
    match kind {
        AlertKind::Setup => ["/System/Library/Sounds/Sosumi.aiff", "/System/Library/Sounds/Pop.aiff"],
        AlertKind::Won => ["/System/Library/Sounds/Purr.aiff", "/System/Library/Sounds/Bottle.aiff"],
        AlertKind::Lost => ["/System/Library/Sounds/Basso.aiff", "/System/Library/Sounds/Frog.aiff"],
        AlertKind::Expired => ["/System/Library/Sounds/Tink.aiff", "/System/Library/Sounds/Morse.aiff"],
    }
}

/// Non-blocking, best-effort — a missing sound file or a non-macOS host
/// (no `afplay`) just means silence, never an error that interrupts the
/// monitor. Matches v1's own defensive `Path(sound).exists()` check.
pub fn play(kind: AlertKind) {
    let [primary, fallback] = sounds_for(kind);
    let sound = if Path::new(primary).exists() {
        primary
    } else if Path::new(fallback).exists() {
        fallback
    } else {
        return;
    };
    let _ = Command::new("afplay").arg(sound).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_alert_kind_maps_to_a_distinct_primary_sound() {
        let kinds = [AlertKind::Setup, AlertKind::Won, AlertKind::Lost, AlertKind::Expired];
        let primaries: Vec<&str> = kinds.iter().map(|k| sounds_for(*k)[0]).collect();
        let unique: std::collections::HashSet<&&str> = primaries.iter().collect();
        assert_eq!(unique.len(), primaries.len(), "expected all primary sounds to be distinct");
    }

    #[test]
    fn no_sound_here_overlaps_v1s_set() {
        let v1_sounds = ["Funk.aiff", "Glass.aiff", "Hero.aiff", "Ping.aiff"];
        for kind in [AlertKind::Setup, AlertKind::Won, AlertKind::Lost, AlertKind::Expired] {
            for path in sounds_for(kind) {
                for v1 in v1_sounds {
                    assert!(!path.ends_with(v1), "{path} collides with v1's {v1}");
                }
            }
        }
    }
}
