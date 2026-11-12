# Conflict replay

An offline harness that rebuilds the corpus of real merge conflicts between PRs and `main`, runs candidate resolvers over it, and builds and tests a sample of what they produce. It answers "how many conflicts would this resolver have retired, and was it right?" without changing anything in production.

The method is the one in the appendix of [Why merge conflicts are so frequent in mono](../../../../docs/investigations/merge-conflict-sources-and-remedies.md), starting from GitHub rather than the engine's ledger. The resolver under test is described in [Keep-both conflict resolution](../../docs/conflict-keep-both-resolution.md).

## What it does

| Script       | Step                                                                                                                                                                                                                                                                             |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `corpus.py`  | `fetch-prs`: one GraphQL pass over every PR with its force-push events. `clone`: a bare clone plus every PR head and every force-pushed-away commit, fetched by SHA. `replay`: `git merge-tree` of each pre-push head against the `main` commit that preceded the push.          |
| `resolve.py` | `run`: every content-conflicted file through the keep-both resolver (both scopes) and through Mergiraf. `report`: files and whole states resolved, and agreement with the resolution that was pushed. `jj-check`: the same conflicts re-created with jj, the engine's own merge. |
| `gate.py`    | `sample`: a seeded sample of fully resolved states. `run`: check out each merged tree with the resolver's output, format, `bazel build` and `bazel test` the owning targets. `report`: the outcome table.                                                                        |

A **conflict state** is one distinct (PR, pre-push head) that conflicts with `main`. A push counts when it moved the PR's branch point forward along `main`; an amend, or a restack of a branch stacked on another PR, does not.

The keep-both resolver is not reimplemented here. `resolve.py` calls `keep_both_replay`, a small binary built from the engine's resolver crate, so the replay measures the code the engine runs. `git merge-tree` writes diff3 conflict markers, which that code reads as one of jj's own marker styles; `jj-check` confirms the verdicts carry over to conflicts materialised by jj.

## Running it

Requirements: `git` 2.38 or newer, an authenticated `gh`, `python3`, `rustfmt` on `PATH`, and `bazel`. `jj` is needed for `jj-check` and a [Mergiraf](https://mergiraf.org) binary for the Mergiraf column; both are optional.

All state goes under one work directory, which must be outside any checkout. Expect about 1 GB before the gate and tens of GB of Bazel output after it.

```sh
WORK=/path/outside/any/checkout
HERE=tools/boss/scripts/conflict-replay

python3 $HERE/corpus.py --work-dir "$WORK" fetch-prs
python3 $HERE/corpus.py --work-dir "$WORK" clone
python3 $HERE/corpus.py --work-dir "$WORK" replay

bazel build //tools/boss/engine/deterministic-resolvers:keep_both_replay
python3 $HERE/resolve.py --work-dir "$WORK" run \
  --union-bin "$PWD/bazel-bin/tools/boss/engine/deterministic-resolvers/keep_both_replay" \
  --mergiraf /path/to/mergiraf
python3 $HERE/resolve.py --work-dir "$WORK" report --reasons 15
python3 $HERE/resolve.py --work-dir "$WORK" jj-check \
  --union-bin "$PWD/bazel-bin/tools/boss/engine/deterministic-resolvers/keep_both_replay"

python3 $HERE/gate.py --work-dir "$WORK" sample
python3 $HERE/gate.py --work-dir "$WORK" run \
  --union-bin "$PWD/bazel-bin/tools/boss/engine/deterministic-resolvers/keep_both_replay"
python3 $HERE/gate.py --work-dir "$WORK" report --details
```

`gate.py run` is resumable and takes hours: each state is a different historical revision, so the crates that own the resolved files are rebuilt every time. It builds in a `git worktree` of the scratch clone with its own Bazel output base, never in the checkout it is run from.

The gate judges the built-in keep-both scope unless told otherwise. `gate.py --candidate mergiraf` or `--candidate lines`, given before the subcommand, samples, builds and reports the states that candidate resolved instead, keeping its results apart from the others.

## How to read the results

- **Files resolved** counts content conflicts outside lockfiles. Lockfiles are left out because the engine regenerates them; add/add, modify/delete and rename conflicts are counted as unresolved for every candidate.
- **States resolved** requires every conflicted file in the state to be resolved. "With lockfile regeneration" also accepts lockfiles, on the assumption that the engine's existing lockfile tier succeeds.
- **Agreement with the pushed resolution** compares a candidate's output, after `rustfmt`, with the file in the commit that replaced the conflicted head. It is only counted where `main` did not change the file between the commit replayed against and the commit the push landed on. "Different" does not mean wrong: the agent that resolved the conflict often also adapted the code to an API the other side had changed, which a textual resolver cannot do and the build gate exists to catch.
- **The gate** attributes a failure to keeping both sides only when the pushed resolution passes the same targets on the same host. A historical revision that no longer builds here, or a test that fails either way, is reported separately and left out of the pass rate.

## Results of the 2026-11-12 run

Recorded when keep-both was extended from `mod.rs` and `lib.rs` to every Rust file. The corpus covers every PR in `spinyfin/mono` up to that day.

**Corpus.** 1,124 conflict states holding 1,947 conflicted files: 273 lockfiles the engine regenerates, 82 add/add, modify/delete and rename conflicts, and 1,592 content conflicts, of which 1,320 are Rust.

**Files resolved.**

| Candidate                                         | Content-conflicted files |  Rust files | `rustfmt` accepts the Rust result |
| ------------------------------------------------- | -----------------------: | ----------: | --------------------------------: |
| Keep-both, whole items and imports (the built-in) |              207 (13.0%) | 207 (15.7%) |                        207 of 207 |
| Keep-both, any whole lines (per-path rule scope)  |              499 (31.3%) | 403 (30.5%) |                        403 of 403 |
| Mergiraf 0.20.0                                   |              549 (34.5%) | 450 (34.1%) |                        444 of 450 |

**Whole states resolved.**

| Candidate                                         |       Alone | With lockfile regeneration |
| ------------------------------------------------- | ----------: | -------------------------: |
| Lockfile regeneration only (the existing tier)    |             |                180 (16.0%) |
| Keep-both, whole items and imports (the built-in) |  107 (9.5%) |                 109 (9.7%) |
| Keep-both, any whole lines (per-path rule scope)  | 206 (18.3%) |                224 (19.9%) |
| Mergiraf 0.20.0                                   | 203 (18.1%) |                221 (19.7%) |

The keep-both rows do not include the 180 lockfile-only states, so the built-in takes the deterministic tiers from 180 states to 289 (25.7%). Counting lockfile regeneration for both, 92 states are resolved by the built-in and by Mergiraf, 17 only by the built-in and 129 only by Mergiraf.

**Agreement with the pushed resolution.**

| Candidate                                         | Comparable files |   Identical | Same lines, other order |   Different |
| ------------------------------------------------- | ---------------: | ----------: | ----------------------: | ----------: |
| Keep-both, whole items and imports (the built-in) |              179 | 140 (78.2%) |                2 (1.1%) |  37 (20.7%) |
| Keep-both, any whole lines (per-path rule scope)  |              446 | 321 (72.0%) |               40 (9.0%) |  85 (19.1%) |
| Mergiraf 0.20.0                                   |              503 | 307 (61.0%) |               47 (9.3%) | 149 (29.6%) |

**Why the built-in declined.** Of the 1,113 Rust files it left alone: 776 have a conflict that edits lines already there, 266 insert something that is not a whole item (a statement, a match arm, a field), 34 insert a fragment that does not stand on its own (17 with a bracket the fragment did not open, 17 with a comment or attribute attached to no item), 20 remove or rewrite an import, and 17 have both sides adding the same thing. Mergiraf resolved 7 of those 17.

**Conflicts re-created with jj.** 472 Rust files from 288 states (every state with a file the built-in resolved, and 100 sampled others), merged by jj 0.26.0 instead of `git merge-tree`: 261 declined under both, 206 resolved under both with byte-identical output, 5 not conflicted under jj at all. No file got a different verdict.

**The gate.** 50 of the 107 states the built-in resolves alone, chosen with seed 20261111, each built and tested on the targets that own the resolved files:

| Outcome                                                         | States |
| --------------------------------------------------------------- | -----: |
| Builds and passes its tests                                     |     44 |
| Does not compile; the pushed resolution does                    |      4 |
| Compiles; the same tests fail for the pushed resolution as well |      2 |

That is 44 of the 48 states with a verdict (92%). All four failures are compile errors, the kind the push gate refuses before anything reaches GitHub, and none is a tree that compiles and then fails a test the pushed resolution passes. In each, one branch changed an interface the other branch's new code uses: a trait gained a required method, a trait method changed its parameters, a constructor gained an argument, an import was dropped as unused. In two of the four the stale code is in a file that merged without any conflict, so no per-file resolver could have seen it.

Two qualifications apply to the gate. Revisions before mono#3091 (2026-10-07) pin a Rust release whose proc-macro libraries do not load on the macOS version of the host the gate ran on, so 42 of the 50 states were built with the host's Rust release for both the keep-both tree and the pushed-resolution control. And tests were attempted up to three times, because the engine's largest test shards time out when several builds share the machine.

The whole-lines scope and Mergiraf were not gated in this run. Their numbers above are textual results only; `gate.py --candidate` builds and tests them the same way.

## Limits

- The corpus only contains conflicts that were resolved by a force-push. A conflict on a PR that was closed or is still open and conflicted is missing.
- `git merge-tree` and jj do not always split a merge into the same hunks. `jj-check` measures how often that changes the verdict.
- The gate builds with `bazel build` and `bazel test`. It does not run clippy, which the push gate does.
- Mergiraf's output is checked for syntax and compared with the pushed resolution, but not built.
