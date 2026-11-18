# Codex progression spawns never attached a rollout

Every Codex progression spawn failed with `no driver hook or session event arrived within 120s after prompt delivery`. Codex did not trust the run's working directory, so it replaced its composer with its folder-trust dialog, and the brief the launcher had delivered was lost. That is fixed.

Progression itself did not stop. A progression run gets its driver per item, runs given Claude worked throughout, and the items whose Codex launch failed were retried six hours later, when their cooldown ended. Nothing showed that wait, and a failed launch was charged to the stuck item, so a second one would have parked the item on a question. A failed launch is no longer charged to the item, and the wait between attempts is reported.

This answers the first question [progression-slot-reuse-2026-11-17.md](progression-slot-reuse-2026-11-17.md) left open: why an armed Codex ingress never attached a rollout. Source paths below are relative to `tools/boss/`.

## Why no rollout was ever written

1. A progression run holds no checkout. Its working directory is engine scratch under the system temp directory (`engine/execution-contracts/src/lib.rs:2704`, created at `engine/dispatch/src/coordinator/progression_spawn.rs:70`). On macOS that directory is `/var/folders/…/T/boss-progression-scratch/<execution id>`, and `/var/folders` is a symlink to `/private/var/folders`.
2. The per-run Codex config trusted the workspace under the path as the engine spelled it: one `[projects."<workspace>"] trust_level = "trusted"` entry (`render_config_toml`, `engine/driver/src/codex.rs:884`).
3. Codex looks its project trust up under the working directory it resolves for itself, which is the physical path, `/private/var/folders/…`. It found no entry.
4. Codex draws its composer before it acts on that. The first frames of an untrusted launch are the banner, a `loading` line and `› Ask Codex to do anything`, exactly as in a trusted one. Only then does it clear the screen and open its folder-trust dialog.
5. The launcher judged that first composer ready (`ready_for_prompt`, `engine/driver/src/initial_prompt.py:152`, on the markers at `engine/driver/src/codex/pane_monitor.rs:76`) and delivered the brief: paste, then Return (`engine/driver/src/initial_prompt.py:489`). Codex declares no paste-landed marker, so any repaint counted as the composer taking the paste (`engine/driver/src/initial_prompt.py:193`).
6. The brief was lost whichever screen received it. Both outcomes were measured. When the keystrokes reached the provisional composer, the dialog replaced it and stayed on screen unanswered. When they arrived as the dialog came up, the dialog discarded the paste and Return selected its highlighted row, `› 1. Trust and continue`: Codex saved the trust decision and opened an empty composer.
7. Codex writes a rollout file only once a turn starts. No turn started, so the run's transcript directory stayed empty and the armed ingress had nothing to discover. Discovery itself was sound: it canonicalizes the rollout's `cwd` before comparing it with the workspace (`engine/driver/src/discovery.rs:653`), so it would have attached a rollout had one existed.
8. The turn-start wait ran out (`engine/execution-runner/src/runner/spawn_confirmation.rs:226`) and the spawn failed with `TurnDidNotStart` (`engine/execution-contracts/src/spawn_confirmation.rs:27`).

The launcher's delivery log read `ready`, `paste_sent`, `paste_landed`, `submit_sent` on every one of these spawns, which is why they looked delivered.

### Why only progression

Every other Codex worker runs in a cube workspace, whose path contains no symlink, so the engine's spelling and Codex's are the same string. Progression is the one launch whose working directory is under the temp directory. The profile is not the cause: a standard-profile launch with `--sandbox danger-full-access` in a symlinked directory fails the same way.

Hook trust was never affected. It already attests against the physical path (`write_hooks_and_attest` canonicalizes the config path, the home and the hooks `cwd`).

## Measurements

Each row is one launch of the real CLI through the production launcher, in an isolated `CODEX_HOME`, in a working directory reached through a symlink, stopped after 15 to 25 seconds. The home's auth was an API key that no service accepts, so no model call could succeed.

| codex-cli | Trust entry                                              | Screen when the run was stopped   | Rollout written | Trust saved by Codex |
| --------- | -------------------------------------------------------- | --------------------------------- | --------------- | -------------------- |
| 0.169.0   | engine's spelling only                                   | empty composer                    | no              | yes, physical path   |
| 0.160.1   | engine's spelling only                                   | empty composer                    | no              | yes, physical path   |
| 0.169.0   | engine's spelling only, standard profile                 | empty composer                    | no              | yes, physical path   |
| 0.169.0   | engine's spelling only, model endpoint closed            | the dialog, unanswered            | no              | no                   |
| 0.169.0   | engine's spelling only, dialog held as a starting marker | the dialog, unanswered            | no              | no                   |
| 0.169.0   | physical path                                            | brief submitted, turn in progress | yes             | no                   |
| 0.160.1   | physical path                                            | brief submitted, turn in progress | yes             | no                   |
| 0.169.0   | both spellings, as now rendered                          | brief submitted, turn in progress | yes             | no                   |

The launcher logged `ready`, `paste_sent`, `paste_landed`, `submit_sent` in every row. In the first it reached `ready` 629 ms after start, `paste_landed` 3 ms after the paste and `submit_sent` 1 ms after that. Across the untrusted rows `ready` came between 592 and 708 ms.

Replaying the captured terminal output of the untrusted launches through the launcher's own screen model gives the same order of frames on both releases: banner, then banner with `loading` and the `›` composer, then an empty screen, then the dialog's question, then its `› 1. Trust and continue` row.

The dialog as 0.160.1 draws it (0.169.0 replaces `2. Quit` with `2. Back to Agent Command Center` and `esc quit` with `esc back`):

```
  Folder access
  /private/…/ws

  Trust this folder? Codex can read, edit, and run files here, subject to your permission settings. …

› 1. Trust and continue
  2. Quit

  enter continue · esc quit
```

## What progression did after the failures

Progression did not stop, and the Codex launch did not recover. The driver did.

A progression run is not pinned to a driver (`engine/storage-values/src/pool_policy.rs:146`). Each execution gets one when it is minted: the item's own pin, else the product default, else the driver traffic split, placed by a hash of the item id (`decide_execution_driver`, `engine/work-db/src/driver_allocation.rs:469`). So some stuck items get Codex and some get Claude.

The engine's recorded token spend lists every progression run that ever did any work. Between 04:00 and 20:30 CST on 2026-11-17 there were twelve progression runs. Six recorded tokens, all completed, and all six ran on Claude:

| Started  | Model             | Output tokens |
| -------- | ----------------- | ------------- |
| 04:06:30 | claude-fable-5-2  | 8,416         |
| 17:36:16 | claude-sonnet-5-5 | 967           |
| 17:41:17 | claude-fable-5-2  | 28,309        |
| 17:46:19 | claude-fable-5-2  | 3,714         |
| 17:46:59 | claude-sonnet-5-5 | 558           |
| 18:49:07 | claude-sonnet-5-5 | 3,241         |

The other six recorded none. They include the four Codex spawns that failed at 11:35, 11:37, 11:39 and 11:41. These six are also the only progression runs with recorded spend since progression spawning was added, so no Codex progression run has ever recorded a token.

The four runs from 17:36 to 17:47 were queued by the sweep passes at 17:36:16, 17:41:17 and 17:46:18: one item, then two, then one. Those are the first passes after 17:35, 17:37, 17:39 and 17:41, six hours after each of those failures. That is the cooldown ending for the same four items; the items themselves were not compared. Between 11:41 and 17:36 the engine was not trying because it was backing off, as designed:

- **Cooldown.** A failed run starts the item's `progression.cooldown` like any other run (`in_cooldown_sql`, `engine/work-db/src/progression_candidates.rs:533`). The engine's calibration reports it as 21,600 seconds.
- **Nothing reported it.** An item inside that cooldown looks the same as an item nothing will ever retry. The one fleet-level signal, the `pre_start_spawn_failure_streak` health alert, is held in memory, is dropped by an engine restart, and says that spawns are failing, not that a retry is scheduled.

What changed between the 11:41 failure and the 17:36 success is therefore the driver those items were given on the retry, not the Codex launch:

- No change to the Codex launch or rollout discovery path merged in that window. The last one before it was the slot-ownership fix, which merged at 10:55, before the 11:35 failures.
- The failure reproduces now, on the host's installed CLI, in the same launch shape (see Measurements, taken between 18:04 and 19:27 the same day).
- Allocation is deterministic for a fixed split, so the same item moving from Codex to Claude means a pin or the traffic split changed between the two mints. Which one is recorded per execution in `execution_driver_decisions` and was not available to this investigation.

### The path that does give up

Two failed runs in a row stop the sweep nominating an item (`infrastructure_failing_sql`, `engine/work-db/src/progression_candidates.rs:554`). The next pass files "Progression could not unstick this item. Park it, archive it, or keep trying?" and parks the item `awaiting_operator_answer` (`infrastructure_failed_items` and `file_question`, `engine/work-db/src/progression_ladder.rs:443`), and a parked item is not nominated again until the question is answered. A failed spawn counted as such a failure, because the dispatcher records it as a `failed` execution (`engine/dispatch/src/coordinator/progression_spawn.rs:296`).

The four items above escaped it only because their second attempt was given Claude. An item that keeps being given Codex fails twice and is parked on a question that blames the item.

This has happened. The calibration counts five questions filed by the engine's ladder in its four-week window. Its other rungs need two or three completed runs on one item, and only six runs have ever completed, each on a different item, so those five can only have been filed for runs that failed. None is open at the time of writing: the stuck report has no item in its `progression_failing` bucket.

## What changed

- **Trust under every spelling.** The rendered config trusts the workspace under the engine's spelling and its physical path, using the helper the Grok driver already keys its folder trust by (`trust_path_variants`, `engine/driver/src/scoped_home.rs:352`, called from `render_config_toml`, `engine/driver/src/codex.rs:884`). This applies to every Codex worker kind.
- **The launcher does not start delivery while the dialog is on screen.** `Trust this folder?` is a Codex launcher starting marker (`engine/driver/src/codex.rs:882`), and a launcher that gives up on a startup screen now names it: `driver did not become ready for initial brief within 30s; it was still showing 'Trust this folder?'`. This is a partial guard. It holds only when the dialog is already up as the launcher judges readiness; a brief delivered into the composer Codex draws first is lost all the same, as the fifth row of the table shows. The trust entries are what keep the dialog away.
- **A failed launch is not an attempt on the item.** A progression run that ended `failed` with a pre-start failure counted is left out of the two runs the infrastructure-failure rung counts (`launch_failed_sql`, `engine/work-db/src/progression_candidates.rs:195`). It still starts the cooldown, so the backoff between attempts is unchanged, and the item is retried each time the cooldown ends. A run that started and then died is counted as before.
- **Questions filed for failed launches are withdrawn.** Each sweep pass withdraws an open infrastructure-failure question whose item is no longer infrastructure-failing and restores the item as "keep trying" does (`retract_unwarranted_questions`, `engine/work-db/src/progression_ladder.rs:551`). None is open today, so this does nothing on the first pass after upgrade unless another item is parked that way before then.
- **The backoff is reported.** After every pass the sweep publishes how many stuck items are waiting out a cooldown that a failed launch started, when the first is retried, and the latest error (`WorkDb::progression_launch_backoff`, `engine/work-db/src/progression_candidates.rs:866`). The engine health report shows it as a `progression_launch_backoff` warning (`engine/core/src/progression_sweep.rs:105`). It is read from the database on each pass, so a restart does not lose it, and it clears once an agent starts or no item is waiting. Had it existed, the six hours after 11:41 would have read `Progression is backing off: its agent failed to start for 4 stuck items; next attempt in about 6 hours` instead of nothing.

No limit, timeout or cooldown value changed. The 120-second discovery deadline, the 300-second driver-start grace, the cooldown and the cap of two are as they were. Progression's driver is still decided the way every other kind's is; nothing here moves it off Codex.

## Regression evidence

All run against the checksum-pinned codex-cli 0.160.1 where a real CLI is involved.

- `a_progression_launch_in_a_symlinked_workspace_attaches_its_rollout` provisions a progression run with the production path in a symlinked working directory, starts it with the production spawn line and launcher, and requires the engine's own armed discovery to attach the rollout and the rollout to carry the brief. Before the fix this is the incident: no rollout.
- `codex_asks_to_trust_a_folder_stamped_only_under_the_engines_spelling` removes the physical-path trust entry from that config and requires the real CLI to show the dialog, the launcher's readiness check with the production Codex markers to refuse delivery while it is up, Codex to have saved no trust decision, and no rollout to exist. It delivers nothing, so it does not depend on which screen a brief would have reached.
- `a_driver_that_never_becomes_ready_is_reported_with_the_startup_screen_that_held_it` covers the launcher's failure message, and `terminal_relay_preserves_followups_resize_and_exit_status` replays the dialog as a screen the launcher must not deliver to.
- `a_workspace_reached_through_a_symlink_is_trusted_under_its_physical_path_too` covers the base, progression and review-guide profiles.
- `failed_launches_are_retried_after_each_cooldown_and_never_file_the_question`, `a_failed_launch_between_two_runs_that_died_neither_counts_nor_hides_them`, `a_question_filed_for_failed_launches_is_withdrawn_and_the_item_runs_again`, `a_question_filed_for_runs_that_started_and_died_stays_open` and `the_launch_backoff_names_the_waiting_items_the_next_attempt_and_the_error` cover the ladder.
- `a_failed_progression_spawn_fails_the_run_but_not_the_item_and_removes_the_scratch_dir` ties the dispatcher's own record of a failed spawn to that classification.
- `a_pass_reports_the_launch_backoff_and_the_loop_publishes_it` and `progression_launch_backoff_is_on_the_health_report_until_the_sweep_clears_it` cover the notice.

## Still open

The other two questions from the slot-reuse investigation are not answered here: which cleanup first freed the colliding claim, and why one spawn-confirmation future resumed tens of minutes after its paste. Both need engine log lines from that interval.
