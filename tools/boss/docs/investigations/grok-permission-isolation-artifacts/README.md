# Grok permission isolation — probe artifacts

Companion to [`../grok-permission-isolation-2026-07-27.md`](../grok-permission-isolation-2026-07-27.md).

| Path                    | Contents                                                                          |
| ----------------------- | --------------------------------------------------------------------------------- |
| `grok_version.txt`      | Pinned CLI version under test                                                     |
| `fixtures/`             | Config / Claude settings / sandbox.toml templates                                 |
| `scripts/run_probes.sh` | Reproducible headless harness (groups `a` / `b` / `c` / `parse`)                  |
| `evidence/`             | Sample inspect JSON, headless results, sandbox events from the investigation host |

## Re-run

```bash
./scripts/run_probes.sh all
```

Requires a local `grok` binary and a readable auth file (default `~/.grok/auth.json`), which is **byte-copied** into throwaway homes — never used as a live `GROK_HOME`.

Default scratch root: `~/.cache/grok-perm-isolation-probe` (intentionally **not** under `/tmp`; sandbox profiles always allow writes there).
