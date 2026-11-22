# Antigravity provider quota

The Settings Driver Quota row reads `agy --print /usage --output-format json` in a Boss-provisioned isolated home. This is a local slash command, not a model prompt. The vendor CLI accesses its credential through the same login-keychain file bridge as workers; Boss does not read, copy, or log the token. The CLI auto-updater is disabled and account information is hidden, as for workers.

Measured with the installed agy 1.5.10 on November 20, 2026: the command returned `status: SUCCESS`, `num_turns: 0`, and zero input, output, thinking, cache-read and total tokens. The structured `command.data.groups[].buckets[]` data reports:

| Bucket               | Period              | Value                            | Reset                 |
| -------------------- | ------------------- | -------------------------------- | --------------------- |
| `gemini-weekly`      | Weekly              | Remaining fraction from 0 to 1   | RFC 3339 `reset_time` |
| `gemini-5h`          | Five hours          | Remaining fraction from 0 to 1   | RFC 3339 `reset_time` |
| `3p-weekly`, `3p-5h` | Weekly / five hours | Independent Claude/GPT allowance | RFC 3339 `reset_time` |

Boss displays the Gemini weekly bucket because the agy driver dispatches Gemini models. Used percentage is `(1 - remaining_fraction) * 100`; it is never inferred from Boss token accounting, averaged across groups, or replaced with the five-hour allowance. The captured fixture retains the provider's numeric values and reset timestamps, omitting descriptive prose and unused fields. It reported 0.842303% used, resetting November 25 at 00:52:21 UTC. The CLI also describes usage as proportional to token cost, not an absolute token count.

An isolated authenticated recheck on November 21, 2026 returned the same schema, `SUCCESS`, zero model turns and zero tokens. Its Gemini weekly bucket reported `remaining_fraction: 1` and reset `2026-11-28T23:13:48Z`. A genuine provider reading of full allowance is distinct from missing quota; only the former can produce 0% used.

The parser requires a successful local `usage` command, a unique weekly bucket, and a valid remaining fraction. Missing Gemini quota produces “Quota not reported by agy”; invalid output and command failures produce explicit errors. No missing value defaults to zero. An absent reset time stays absent; an invalid reset time fails parsing.

The quota cache supplies checking/progress states and its normal refresh policy. The probe runs outside worker slots. It does not link a durable transcript directory: quota homes are not recorded on work executions, so the worker home-retention sweep would never expire them. Probes share one `quota-probe` container id (they are serialized). Before provisioning, the probe deletes that container and any leftover `quota-probe-*` siblings from a previous scheme. After the command — and after a provisioning failure — it deletes the container again, having first checked it is under the Boss agy homes root. Provisioning omits the redundant `models` authentication request because `/usage` itself exercises the same credential.

The CLI is started with `--settings` pinning `agentMode` to the empty documented default and `toolPermission` to `request-review`, plus a deny-all `PreToolUse` hook. That fail-closed posture is in place before any turn, so a qualified binary that treated `/usage` as a prompt cannot auto-approve tools in a HOME that holds the keychain bridge. `/usage` itself was measured as a local zero-turn command on agy 1.5.10; the qualified minimum remains 1.5.7.

For a manual authenticated check, build `//tools/boss/engine/driver:agy_quota_probe` with local outputs and invoke that non-test diagnostic executable with a new absolute evidence directory. It places both the home and transcript store below that directory. Never invoke the real agy binary with the developer's home. The diagnostic prints only the quota response; it does not print the credential.

The quiet app fixture `--capture-reveal-state=driver-quota` renders the production section with the captured agy value and the other rows checking. It demonstrates presentation, not a live engine connection.
