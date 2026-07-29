#!/bin/zsh
# Codex exit-code / output-capture probe harness.
#
# Replicates Boss's production Codex spawn exactly:
#   codex exec --json --strict-config --skip-git-repo-check \
#     --sandbox <mode> -m <model> [-c model_reasoning_effort=<e>] "<prompt>" < /dev/null
# with a run-private CODEX_HOME containing a copied auth.json, matching
# tools/boss/engine/driver/src/codex.rs (build_codex_exec_command +
# codex_home_for_run).
#
# Captures BOTH JSONL dialects so they can be diffed:
#   stdout.jsonl  -- `codex exec --json` stdout envelopes (thread.*/turn.*/item.*)
#                    NOTE: Boss does NOT ingest this; in a pane it goes to the tty.
#   rollout-*.jsonl -- CODEX_HOME/sessions rollout (session_meta/event_msg/
#                    response_item). This IS what Boss ingests, and its
#                    function_call_output records are what the model saw.
#
# Usage: run_probe.sh <probe-name> <sandbox-mode> <prompt-file> [effort]
set -u

PROBE="$1"; SANDBOX="$2"; PROMPT_FILE="$3"; EFFORT="${4:-low}"
MODEL="${PROBE_MODEL:-gpt-5.6-terra}"

ROOT="${0:A:h}"
OUT="$ROOT/out/$PROBE"
rm -rf "$OUT"; mkdir -p "$OUT"

# Run-private CODEX_HOME, as Boss does per run.
HOME_DIR="$OUT/codex_home"
mkdir -p "$HOME_DIR/sessions"
cp "$HOME/.codex/auth.json" "$HOME_DIR/auth.json"
chmod 600 "$HOME_DIR/auth.json"
# Minimal config: --strict-config rejects unknown keys, so keep this tiny.
cat > "$HOME_DIR/config.toml" <<CFG
model = "$MODEL"
CFG

# Scratch cwd so workspace-write probes cannot touch the repo.
WORK="$OUT/work"; mkdir -p "$WORK"

{
  print -r -- "probe=$PROBE"
  print -r -- "sandbox=$SANDBOX"
  print -r -- "model=$MODEL effort=$EFFORT"
  print -r -- "codex=$(codex --version)"
  print -r -- "cwd=$WORK"
  print -r -- "codex_home=$HOME_DIR"
} > "$OUT/meta.txt"

cp "$PROMPT_FILE" "$OUT/prompt.txt"

START=$(date -u +%s)
CODEX_HOME="$HOME_DIR" codex exec \
  --json --strict-config --skip-git-repo-check \
  --sandbox "$SANDBOX" \
  -m "$MODEL" \
  -c model_reasoning_effort="$EFFORT" \
  --cd "$WORK" \
  "$(cat "$PROMPT_FILE")" \
  < /dev/null > "$OUT/stdout.jsonl" 2> "$OUT/stderr.txt"
RC=$?
END=$(date -u +%s)

print -r -- "$RC" > "$OUT/codex_exit.txt"
print -r -- "elapsed_s=$((END-START))" >> "$OUT/meta.txt"

# Collect the rollout file(s) codex wrote -- this is Boss's actual ingress.
find "$HOME_DIR/sessions" -name 'rollout-*.jsonl' -exec cp {} "$OUT/" \; 2>/dev/null

print -r -- "probe $PROBE done rc=$RC"
