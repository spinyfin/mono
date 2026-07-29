# Probe prompts

Each block is the verbatim prompt passed as the `codex exec` positional
argument. Consolidated into one file to keep the changeset within the repo
file-count limit; `run_probe.sh` takes a prompt _file_, so split a block back
out to a temp file to re-run a single probe:

````sh
awk '/^## p3_prompt/{f=1;next} /^## /{f=0} f' prompts.md \
  | sed -n "/^```text$/,/^```$/p" | sed "1d;\$d" > /tmp/p3_prompt.txt
````

## p1_prompt

```text
Run this exact shell command, once, in a single shell call:

sh -c 'echo LINE-ONE; echo LINE-TWO; exit 7'

Then reply with exactly one line: "observed_exit=<the exit code you saw>". Do not run any other command. Do not try to fix anything.
```

## p2_prompt

```text
Run this exact shell command, once, in a single shell call:

echo STEP-A && echo STEP-B && sh -c 'echo STEP-C-START; exit 9' && echo NEVER-REACHED

Then reply with exactly one line: "observed_exit=<the exit code you saw>". Do not run any other command. Do not try to fix anything.
```

## p3_prompt

```text
Run this exact shell command, once, in a single shell call:

sh -c 'seq 1 300000; echo TAIL-MARKER-XYZZY; exit 5'

Then reply with exactly two lines:
observed_exit=<the exit code you saw, or NONE>
saw_tail_marker=<YES if you saw the literal text TAIL-MARKER-XYZZY in the output, otherwise NO>
Do not run any other command. Do not retry.
```

## p4_prompt

```text
Run this exact shell command, once, in a single shell call:

sh -c 'for i in $(seq 1 12); do echo tick-$i; sleep 4; done; echo FINAL-LINE; exit 4'

It takes about 48 seconds. Then reply with exactly one line: "observed_exit=<the exit code you saw, or NONE if you never saw one>". Do not run any other command. Do not retry.
```

## p5_prompt

```text
Run this exact shell command, once, in a single shell call:

sh -c 'echo BEFORE-WRITE; touch ./sandbox-probe-file.txt; echo AFTER-WRITE; exit 0'

Then reply with exactly one line: "observed_exit=<the exit code you saw, or NONE>". Do not run any other command. Do not retry. Do not attempt any workaround or escalation.
```

## p6_prompt

```text
Run this exact shell command, once, in a single shell call:

sh -c 'for i in $(seq 1 12); do echo tick-$i; sleep 4; done; echo FINAL-LINE; exit $(( $(od -An -N1 -tu1 /dev/urandom | tr -d " ") % 50 + 10 ))'

The exit code is random and is NOT knowable in advance. Then reply with exactly one line:
observed_exit=<the exit code you actually saw in tool output, or NONE if you never saw an exit code>

It is CORRECT and expected to answer NONE if no exit code appeared. Do not guess. Do not run any other command.
```

## p7_prompt

```text
Run this exact shell command, once, in a single shell call:

sh -c 'echo BEFORE-SIGNAL; kill -KILL $$; echo NEVER-REACHED'

Then reply with exactly one line:
observed_exit=<the exit code you actually saw in tool output, or NONE if you never saw one>
Do not run any other command. Do not retry.
```
