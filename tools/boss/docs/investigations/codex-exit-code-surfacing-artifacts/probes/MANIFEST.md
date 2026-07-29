# Probe manifest

One block per probe: the exact spawn parameters used. Consolidated from
per-probe meta.txt files to keep the changeset within the repo file-count limit.

## p1_short_nonzero

    probe=p1_short_nonzero
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p1_short_nonzero/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p1_short_nonzero/codex_home
    elapsed_s=9

## p2_chain

    probe=p2_chain
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p2_chain/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p2_chain/codex_home
    elapsed_s=12

## p3_bigout_then_fail

    probe=p3_bigout_then_fail
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p3_bigout_then_fail/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p3_bigout_then_fail/codex_home
    elapsed_s=15

## p4_longrun

    probe=p4_longrun
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p4_longrun/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p4_longrun/codex_home
    elapsed_s=59

## p5_readonly_denial

    probe=p5_readonly_denial
    sandbox=read-only
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p5_readonly_denial/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p5_readonly_denial/codex_home
    elapsed_s=15

## p6_hidden_exit

    probe=p6_hidden_exit
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p6_hidden_exit/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p6_hidden_exit/codex_home
    elapsed_s=41

## p7_signal_kill

    probe=p7_signal_kill
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    cwd=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p7_signal_kill/work
    codex_home=/private/tmp/claude-501/-Users-brianduff--local-share-cube-workspaces-mono-agent-044/f6e7daec-c3de-4502-999b-a743623ffa1d/scratchpad/probes/out/p7_signal_kill/codex_home
    elapsed_s=13

## p8_pty_tty

    probe=p8_pty_tty
    sandbox=workspace-write
    model=gpt-5.6-terra effort=low
    codex=codex-cli 0.145.0
    prompt=p1_prompt (same command as p1_short_nonzero)
    note=stdout attached to a real PTY via harness/pty_probe.py, not a pipe.
    note=run inline rather than through run_probe.sh; see harness/README.md.
