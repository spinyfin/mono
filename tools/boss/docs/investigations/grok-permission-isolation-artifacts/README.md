# Grok permission isolation — probe artifacts

Companion to [`../grok-permission-isolation-2026-07-27.md`](../grok-permission-isolation-2026-07-27.md).

| Path                    | Contents                                                                                                                  |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `scripts/run_probes.sh` | Reproducible headless harness (groups `a` / `b` / `c` / `parse`). Writes fixtures into the probe scratch root at runtime. |
| `evidence/a_claude/`    | Redacted inspect samples + trimmed A4/A7 runtime JSON cited by the findings doc                                           |
| `evidence/b_rules/`     | CLI parse-error text + native-tool fail-open sample                                                                       |
| `evidence/c_sandbox/`   | Merged Seatbelt `sandbox-events` proof + fail-closed profile stderr                                                       |

Pinned CLI under test: `grok 0.2.112` (see findings header).

## Re-run

```bash
./scripts/run_probes.sh all
```

Requires a local `grok` binary and a readable auth file (default `~/.grok/auth.json`), which is **byte-copied** into throwaway homes — never used as a live `GROK_HOME`.

Default scratch root: `~/.cache/grok-perm-isolation-probe` (intentionally **not** under `/tmp`; sandbox profiles always allow writes there). Fresh probe results land under `$PROBE_ROOT/results/`; committed files under `evidence/` are curated samples only.
