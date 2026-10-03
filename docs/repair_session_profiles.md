# repair_session_profiles

Source: `src/bin/repair_session_profiles.rs` · One-off data repair, 2026-10-02.

tpo-builder's RTH session had no end until `baf159b`, so each live
`session_profiles` row absorbed post-close and early-Globex trades until
19:00 CT. This rebuilds the rows from recorded ticks.

```mermaid
flowchart TD
    R["session_profiles rows for --symbol since --from"] --> T["that day's 08:30-15:00 CT ticks\n(db::last_rth_volume_profile)"]
    T --> C{"fully recorded?\n(levels::rth_levels_from_ticks)"}
    C -- no --> S["skip, list it"]
    C -- yes --> D{"differs from the row?"}
    D -- no --> U["unchanged"]
    D -- yes --> A{"--apply?"}
    A -- no --> P["print old vs new (dry run)"]
    A -- yes --> W["UPDATE poc/vah/val + IB (08:30-09:00 high/low)"]
```

Run 2026-10-02 on MESZ6 from 2026-09-15: 13 rows repaired (10-01 POC
7730 -> 7690; the rest moved by 0.25-3 points at the value-area edges),
2026-09-23 skipped (not fully recorded). A second dry run reports 0 changes.
Uses the same profile code as the live strategies, so stored and live levels
agree. `MESZ6-GLOBEX` rows are not covered: overnight ticks are only
complete since the recorder watchdog (2026-09-30).
