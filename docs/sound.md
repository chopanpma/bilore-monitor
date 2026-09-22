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
| `play()` | Fires `afplay` of the first sound that exists on disk, `REPEAT_COUNT` times (2026-09-22: 3), `REPEAT_GAP` apart (700ms), on a dedicated OS thread so the caller returns immediately | repeat constants: `repeats_three_times_with_a_gap_that_outlasts_a_short_system_sound`; the subprocess spawn itself isn't tested — verified manually (`afplay /System/Library/Sounds/Sosumi.aiff`) |

## Known scope notes

- macOS-only (`afplay`) — on any other host, or if a sound file is
  missing, `play()` silently does nothing rather than erroring. Matches
  v1's own defensive `Path(sound).exists()` check.
- Non-blocking overall — the repeat loop's `std::thread::sleep` calls
  block that dedicated thread only, never the monitor loop or a Telegram
  send. No tokio dependency introduced; a plain OS thread was simpler than
  wiring an async sleep into an otherwise-sync module.
- **2026-09-22 — repeats 3x, 700ms apart** (user request: "make the v2
  alerts more prolonged... play the sound 3 times"). Applies to every
  `AlertKind` uniformly (Setup/Won/Lost/Expired), not just Setup — if only
  one kind should repeat, that's a follow-on, not done here. This actually
  restores something closer to v1's own behavior — v1's real
  `_alert_zone()` already had a "played twice, or three times if strong"
  pattern; v2 had simplified that away to a single play (see git history),
  not an oversight at the time, now reconsidered.
