# Driver traffic split and API key storage: reference

The Engine Settings pane keeps its captions to one sentence each. This
document is where the detail that used to live in those captions moved to.

## Driver traffic split

The three shares (Codex / Claude / Grok) control what percentage of new
work is sent to each driver. They always sum to 100%; moving one stepper
redistributes the difference across the other two.

- **PR reviews and automation keep their own pinned driver, outside the
  split.** This is the single most useful fact here: it explains why a
  driver with a large configured share can still appear to receive almost
  no work — reviews and automation never draw from the split at all, so a
  high share only ever governs the work-item traffic that does.
- **An explicit per-row driver, or a product's default driver, overrides
  the split** for that row.
- **Changing the split only affects work dispatched afterwards.** Nothing
  already running is reassigned.
- **Shares are renormalised over the drivers eligible for a given
  work-item kind.** For a kind some driver cannot run, its share is
  redistributed proportionally among the remaining eligible drivers, so an
  ineligible driver never receives work it cannot do.
- **Allocation is deterministic per work item** — the same row under the
  same split always lands on the same driver.

## API key storage

Saving a key in Settings stores it and restarts the engine so the new
value takes effect; the Settings value overrides any `ANTHROPIC_API_KEY`
already present in the engine's inherited environment.

Where it's stored depends on the build:

- **Signed release builds** store the key in the macOS Keychain.
- **Ad-hoc dev builds** store it in a private file under Application
  Support instead (Keychain access control does not work reliably for
  unsigned/ad-hoc-signed binaries).

Either way the entry is keyed by `APIKeyStore.service` — check that
service identifier first when debugging where a key actually went (e.g.
inspecting Keychain Access, or finding the Application Support file).
