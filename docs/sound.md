# sound

Source: `src/sound.rs` · New in v2 (2026-09-08), user-requested — a
different local audio alert for v2 than v1's, so the two are
distinguishable by ear alone, not just by reading the Telegram message.

v1's `morning_prep.py::_alert_zone()` plays macOS system sounds via
`afplay` on a real zone touch: Funk/Glass normally, Hero/Ping when all
signals confirm. This module uses an entirely disjoint set of sounds for
v2's own alerts, mapped to alert *kind* rather than v1's confirm/not split:

| Kind | Primary | Fallback |
|---|---|---|
| `Setup` (a new trade locks) | Sosumi | Pop |
| `Won` | Purr | Bottle |
| `Lost` | Basso | Frog |
| `Expired` (day rollover, unresolved) | Tink | Morse |

## Function index

| Symbol | Purpose | Tests |
|---|---|---|
| `sounds_for()` (private) | Maps an `AlertKind` to its primary+fallback `.aiff` path | `every_alert_kind_maps_to_a_distinct_primary_sound`, `no_sound_here_overlaps_v1s_set` |
| `play()` | Fire-and-forget `afplay` of the first sound that exists on disk | not tested — spawns a real subprocess; verified manually (`afplay /System/Library/Sounds/Sosumi.aiff`) |

## Known scope notes

- macOS-only (`afplay`) — on any other host, or if a sound file is
  missing, `play()` silently does nothing rather than erroring. Matches
  v1's own defensive `Path(sound).exists()` check.
- Non-blocking (`Command::spawn()`, not `.output()`/`.status()`) — never
  delays the monitor loop or a Telegram send waiting for playback to finish.
- Unlike v1, there's no "played twice, or three times if strong" pattern —
  each kind just plays once. Simplification, not an oversight.
