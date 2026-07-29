#!/usr/bin/env python3
"""Extract and diff the two Codex JSONL dialects for one probe.

For each probe directory produced by run_probe.sh, prints:
  * STDOUT dialect  (`codex exec --json`): item.completed command_execution
    envelopes, with exit_code / status. NOT ingested by Boss.
  * ROLLOUT dialect (CODEX_HOME/sessions/rollout-*.jsonl): the custom_tool_call
    inputs (showing the model-chosen yield_time_ms / max_output_tokens) and the
    custom_tool_call_output the model actually saw. THIS is Boss's ingress.
  * The model's final answer.
"""
import glob
import json
import os
import sys


def load(path):
    out = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if line:
                try:
                    out.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
    return out


def stdout_dialect(d):
    p = os.path.join(d, "stdout.jsonl")
    if not os.path.exists(p):
        print("  (no stdout.jsonl)")
        return
    for o in load(p):
        if o.get("type") != "item.completed":
            continue
        it = o.get("item", {})
        if it.get("type") != "command_execution":
            continue
        agg = it.get("aggregated_output", "")
        print(f"  command   : {it.get('command','')[:160]}")
        print(f"  exit_code : {it.get('exit_code')!r}")
        print(f"  status    : {it.get('status')!r}")
        print(f"  agg_output: {len(agg)} bytes")
        if agg:
            print(f"  agg_tail  : {agg[-200:]!r}")


def rollout_dialect(d):
    files = glob.glob(os.path.join(d, "rollout-*.jsonl"))
    if not files:
        print("  (no rollout file)")
        return
    recs = load(files[0])
    final = None
    for r in recs:
        p = r.get("payload", {}) or {}
        t = p.get("type")
        # The rollout uses BOTH `custom_tool_call*` and `function_call*` record
        # shapes for the same exec tool. Treating only one shape as "the tool
        # result" silently hides later polls -- including the one that finally
        # carries exit_code.
        if t in ("custom_tool_call", "function_call"):
            print(f"  TOOL CALL [{t}]  name={p.get('name')!r}")
            print(f"    input   : {p.get('input','')[:400]}")
        elif t in ("custom_tool_call_output", "function_call_output"):
            out = p.get("output", [])
            # `output` is sometimes a list of {type,text} blocks and sometimes a
            # bare string. Both shapes occur in practice; normalise to a list.
            blocks = [out] if isinstance(out, str) else out
            print(f"  TOOL OUTPUT (what the model saw) [shape={type(out).__name__}]:")
            for blk in blocks:
                txt = blk if isinstance(blk, str) else blk.get("text", "")
                print(f"    [{len(txt)} bytes] {txt[:800]}")
        elif t == "agent_message":
            final = p.get("message")
    if final:
        print(f"  FINAL ANSWER: {final.strip()[:300]!r}")


def main():
    for d in sys.argv[1:]:
        name = os.path.basename(d.rstrip("/"))
        print("=" * 72)
        print(f"PROBE: {name}")
        meta = os.path.join(d, "meta.txt")
        if os.path.exists(meta):
            print(open(meta).read().strip())
        print("-" * 30, "STDOUT dialect (NOT Boss ingress)")
        stdout_dialect(d)
        print("-" * 30, "ROLLOUT dialect (Boss ingress + model view)")
        rollout_dialect(d)
        print()


if __name__ == "__main__":
    main()
