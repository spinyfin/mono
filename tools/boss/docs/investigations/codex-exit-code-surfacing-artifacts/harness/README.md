# Codex exit-code / output-capture probe harness

Harness and raw transcripts for
[`../../codex-exit-code-surfacing.md`](../../codex-exit-code-surfacing.md).

## Pins

- `codex-cli 0.145.0`
- model `gpt-5.6-terra`, `model_reasoning_effort=low`
- spawn line copied from `tools/boss/engine/driver/src/codex.rs`
  (`build_codex_exec_command`)

## What it does

`run_probe.sh` runs one real `codex exec` with the production flag set and a
run-private `CODEX_HOME` (mirroring `codex_home_for_run`), then captures **both**
JSONL dialects so they can be diffed:

| File              | Dialect                                        | Who reads it in production                  |
| ----------------- | ---------------------------------------------- | ------------------------------------------- |
| `stdout.jsonl`    | `thread.*` / `turn.*` / `item.*`               | **nobody** — in a pane this goes to the tty |
| `rollout-*.jsonl` | `session_meta` / `event_msg` / `response_item` | the engine **and** the model                |

That split is the point of the harness: `exit_code` is reliably present in the
first and only sometimes present in the second.

## Re-running

Requires a working `codex` login (`~/.codex/auth.json`); it is copied into each
probe's private home and never modified.

All seven prompts live in [`prompts.md`](prompts.md), one fenced block each;
`run_probe.sh` takes a prompt _file_, so split the block you want back out first
(that file documents the one-liner). Then:

```sh
./run_probe.sh <probe-name> <workspace-write|read-only> <prompt-file> [effort]
./run_probe.sh p1_short_nonzero workspace-write /tmp/p1_prompt.txt
python3 extract.py out/p1_short_nonzero
```

`extract.py` prints, per probe, the stdout-dialect `command_execution`
envelopes, every rollout tool call (including the model-chosen `yield_time_ms`
and `max_output_tokens`), the tool output the model actually saw, and the final
answer.

Probe 8 instead attaches codex's stdout to a real PTY, isolating the
tty-vs-pipe variable that pane hosting would introduce:

```sh
CODEX_HOME=$PWD/out/p8_pty_tty/codex_home python3 pty_probe.py \
  $PWD/out/p8_pty_tty/stdout_tty.raw \
  codex exec --json --strict-config --skip-git-repo-check \
    --sandbox workspace-write -m gpt-5.6-terra -c model_reasoning_effort=low \
    --cd $PWD/out/p8_pty_tty/work "$(cat /tmp/p1_prompt.txt)"
```

## The probes

Spawn parameters for every probe are in
[`../probes/MANIFEST.md`](../probes/MANIFEST.md).

| Probe                 | Prompt block | Tests                                                              |
| --------------------- | ------------ | ------------------------------------------------------------------ |
| `p1_short_nonzero`    | `p1_prompt`  | short output, `exit 7`                                             |
| `p2_chain`            | `p2_prompt`  | `a && b && c` — which exit code surfaces                           |
| `p3_bigout_then_fail` | `p3_prompt`  | ~1 MB output then `exit 5` — truncation limits and shape           |
| `p4_longrun`          | `p4_prompt`  | ~48 s command; model polls to completion (control)                 |
| `p5_readonly_denial`  | `p5_prompt`  | `--sandbox read-only` write denial                                 |
| `p6_hidden_exit`      | `p6_prompt`  | ~48 s command, **randomised** exit code, `NONE` explicitly allowed |
| `p7_signal_kill`      | `p7_prompt`  | `SIGKILL`                                                          |
| `p8_pty_tty`          | `p1_prompt`  | p1 with stdout on a real PTY                                       |

`p6` is the reproduction of the reported failure; `p4` is its control — same
command, but the model kept polling and did receive `exit_code`. `p6`
randomises the exit code specifically so the model cannot infer it from the
prompt, which an earlier version of `p4` allowed.

## Caveats on the committed artifacts

- `probes/p3_bigout_then_fail/stdout.elided.jsonl` has its ~1 MB
  `aggregated_output` replaced by an in-band `[ELIDED BY HARNESS: …]` marker
  for repo size. Every other capture is verbatim.
- `codex_home/` directories are **not** committed (they contain a copied
  `auth.json` and ~40 MB of cache each).
- Probe output is nondeterministic in wording; the fields the writeup relies on
  (`exit_code`, `status`, `original_token_count`, `Script running` /
  `Script completed`) were stable across runs, but the model's chosen
  `yield_time_ms` / `max_output_tokens` and its result projection vary per run —
  that variability is itself Finding 4.
