//! Worker-rule sections shared by the control and the `slim_worker_brief`
//! renderers in [`crate::worker_setup`].
//!
//! Each section lives here once and both renderers concatenate it, so an edit
//! to a shared rule cannot reach one experiment arm and miss the other. Only
//! the intentional differences between the arms stay in `worker_setup`.

/// The evidence-section and ships-inert bullets of the PR deliverable rules.
pub(crate) fn evidence_and_inert_bullets(boss: &str) -> String {
    format!(
        "- **Every PR body must contain an evidence section.** Show concretely how\n\
           the change achieves its stated intent: commands run and their results,\n\
           repeated-run counts for flake fixes, measurements, before/after behavior,\n\
           and screenshots registered via `{boss} attach`, as applicable. Describe\n\
           registered screenshots by caption and what you verified; keep attachment\n\
           URLs out of the PR body. A bare \"tests pass\" is not enough. Where evidence\n\
           could not be gathered, state the specific limitation and what would be\n\
           needed to gather it. Distinguish observed results from expectations.\n\
         - **Declare work that ships inert prominently in the PR body.** If your change\n\
           does not take effect on deploy (a default-off flag, per-environment config,\n\
           unscheduled task, missing capability, or other human action), state that it\n\
           ships inert, the specific action that makes it live, and who can take it.\n\
           Treat this as disclosure, not an approval step or merge gate.\n"
    )
}

/// Bullets telling the worker to print the PR URL and confirm the published head.
pub(crate) const PR_URL_AND_CONFIRM_BULLETS: &str = "- Print the PR URL on its own line as the last thing in your final response.\n\
         - Confirm the published PR contains your commit with `gh pr view` and\n\
           its head SHA; pass `-R owner/repo` in a jj workspace (resolve the slug\n\
           from the PR URL or `jj git remote`).\n";

/// The "Checking your own PR's state" section, through the failed-command paragraph.
pub(crate) fn pr_state_section(boss: &str, cube: &str) -> String {
    format!(
        "## Checking your own PR's state\n\
         \n\
         The engine already stores most of what you'd otherwise ask GitHub for.\n\
         These read your own PR only — never another run's — and cost one\n\
         local round trip against the engine, not a GitHub API call:\n\
         \n\
         - `{boss} pr status` — includes your resolved `pr_url`, the\n\
           cheapest way to answer \"do I already have a PR?\" before\n\
           deciding between `{cube} pr create` and `{cube} pr update`. Prefer\n\
           this over `{boss} context`'s `task.pr_url` field: that field is\n\
           NULL for a revision task by design (the chain root owns the PR,\n\
           not the revision), so it reads as \"no PR\" even when one exists.\n\
           `{boss} pr status` also returns `mergeable`, `merge_state_status`,\n\
           `head_sha`, and `observed_at` for your own PR, e.g.:\n\
           ```sh\n\
           {boss} pr status --json\n\
           ```\n\
           This is the engine's **last stored observation from the merge poller,\n\
           not live GitHub truth** — `observed_at` (Unix epoch seconds) is\n\
           the timestamp of that observation, not of your call. Right after\n\
           a push, the stored snapshot usually still reflects the *pre-push*\n\
           state. If you need current state (e.g. right after `{cube} pr\n\
           update` to see whether the push cleared a conflict), pass\n\
           `--refresh` for one bounded, rate-limited live check:\n\
           ```sh\n\
           {boss} pr status --refresh --json\n\
           ```\n\
           A refresh can be silently throttled (`refresh_throttled: true`\n\
           in the response) if the engine-wide budget is exhausted — in\n\
           that case you still get the last stored snapshot, never an error\n\
           or a hang. Never loop calling `--refresh` waiting for a state\n\
           change: the engine's own merge poller is what watches your PR to\n\
           green after you push. Do not babysit CI with `gh pr checks` or\n\
           `gh pr view` loops either. Finish required post-push artifacts, then\n\
           complete the run using its enabled completion channel. Required checks\n\
           are effectively green at SUCCESS, NEUTRAL, or SKIPPED; still-running\n\
           and human-gated checks do not block completion. A genuinely failed\n\
           check must be classified using the CI failure classification below;\n\
           a CI-fix revision is no substitute for a fix you can make in this run.\n\
         - `{boss} pr body` — the PR title and body/description the\n\
           engine snapshotted when this run started, for a read-modify-write of\n\
           the description without a `gh pr view` round trip. If the\n\
           response's `body` is null (`--json`) or says \"(none stored)\"\n\
           (text), it means this run began a brand-new PR flow — there is\n\
           nothing to diff against yet, you are about to write the first\n\
           description via `{cube} pr\n\
           create --body-file`. It does NOT mean the PR has an empty\n\
           description; an intentionally empty one is stored as `\"\"`, not\n\
           null. Only fall back to `gh pr view --json body` if you must\n\
           confirm the null case is not a fetch failure rather than a\n\
           new-PR flow.\n\
         \n\
         If a `{boss}` command fails to run at all — not found, or it exits\n\
         non-zero without answering — that is a finding, not noise. Say so\n\
         explicitly in your final response and name the command. Do NOT\n\
         quietly drop the step and carry on: a create-vs-update decision\n\
         made without `{boss} pr status` is a decision made blind, and\n\
         nobody downstream can tell that from a clean transcript.\n"
    )
}

/// The "Your workspace" section.
pub(crate) fn workspace_section(workspace: &dyn std::fmt::Display, lease: &str) -> String {
    format!(
        "## Your workspace\n\
         \n\
         - Workspace path: `{workspace}`\n\
         - Cube lease id: `{lease}`\n\
         \n\
         Lease held for the lifetime of this run. Do not lease, release,\n\
         or mutate cube state.\n"
    )
}

/// The "VCS" section and its inline-commit-message subsection.
pub(crate) fn vcs_section(cube: &str, config_dir: &str) -> String {
    format!(
        "## VCS\n\
         \n\
         Use `jj` for all VCS. Do not invoke `git` directly except via `gh`.\n\
         \n\
         - `jj git fetch` to sync; `jj new main@origin` for a fresh task;\n\
           `jj edit <bookmark>` to resume.\n\
         - `jj describe -m '...'` to set commit messages;\n\
           `jj bookmark set <name> -r @` to create or advance a bookmark.\n\
         - **NEVER push branches or open PRs with bare VCS commands** (`jj git push`,\n\
           `git push`, `gh pr create`). A PreToolUse hook blocks these. Use:\n\
           - `{cube} pr create --branch <name>` — new PR (pushes branch + opens PR, jj-aware, no GIT_DIR needed)\n\
           - `{cube} pr update --branch <name>` — existing PR (pushes new commits to it)\n\
         - Never `jj git push --deleted` or `git push --delete`\n\
           without explicit user approval.\n\
         - **Never run raw `git` against cube's shared source store** — the\n\
           checkout your workspace's `.jj/repo` pointer names, under cube's repos\n\
           root — whether by `cd`-ing into it or by pointing `GIT_DIR`, `--git-dir`\n\
           or `-C` at it. A PreToolUse hook refuses every git write there and every\n\
           shallow `fetch`/`pull`/`clone` (`--depth`, `--shallow-since`, …) that\n\
           would touch it: one such fetch turns the shared store into a shallow\n\
           clone and breaks every dispatch for that repository. Read-only git\n\
           against the store (`log`, `show`, `diff`, `rev-parse`, …) is allowed.\n\
           To reproduce a CI script that runs raw git, clone into a throwaway\n\
           directory under your own temp dir and run it there.\n\
         - `{config_dir}/` is gitignored by the engine. Do not force-track\n\
           or commit anything inside it (no `--force`,\n\
           no `jj file track {config_dir}/...`).\n\
         \n\
         ### Commit messages must be inline\n\
         \n\
         Always pass `-m \"…\"` to `git commit`, `git rebase`, `jj commit`,\n\
         `jj describe`, and amend/squash flows (`git commit --amend`,\n\
         `jj squash`). The worker environment has no usable\n\
         `$EDITOR` — commands that fall through to one fail. Fix by\n\
         re-running with `-m`. For splitting on jj 0.26 through 0.46, use\n\
         `jj split --config 'ui.editor=\"true\"' <paths>`, then\n\
         `jj describe -r <revision> -m \"message\"` on each half.\n"
    )
}

/// Heading and introduction of the `origin` shared-object-store section.
const ORIGIN_HEAD: &str = "### `origin` is the real GitHub upstream (shared object store)\n\
         \n\
         Every cube workspace is a **secondary jj workspace** that SHARES one\n\
         object store with its siblings — there is no per-workspace clone. That\n\
         store has a single `origin` remote pointing at the real GitHub upstream.\n\
         \n";

/// First bullet of the `origin` section: only use the cube PR commands.
fn origin_push_only_bullet(cube: &str) -> String {
    format!(
        "- **Only use `{cube} pr create` / `{cube} pr update` for pushes and PR operations**:\n\
           they push to GitHub by URL and resolve `-R <owner/repo>` for `gh`\n\
           automatically, so PR creation Just Works without `.git/` at the root.\n\
           Direct `jj git push`, `git push`, or `gh pr create` calls are blocked\n\
           by a PreToolUse hook in every worker session.\n"
    )
}

/// Remaining `origin` bullets and the push-landed verification recipe.
const ORIGIN_TAIL: &str = "- Because the store is shared, a `jj git fetch` in ANY workspace\n\
           advances the remote-tracking bookmarks (e.g. `main@origin`) seen by\n\
           ALL of them. Don't be alarmed if refs move without you fetching.\n\
         - A solid belt-and-suspenders check that a push actually landed is to\n\
           compare your local commit against GitHub's head sha (do not infer\n\
           success from the push command's own output alone):\n\
         \n\
         ```sh\n\
         # local commit you intended to ship\n\
         jj log -r my-feature --no-graph -T commit_id\n\
         # what GitHub actually has (must match)\n\
         gh api repos/<owner>/<repo>/branches/my-feature --jq .commit.sha\n\
         # for a specific PR head:\n\
         gh api repos/<owner>/<repo>/pulls/<n> --jq .head.sha\n\
         ```\n";

/// The `origin` shared-object-store section. `push_only_bullet` adds the rule
/// restating that only the cube commands push, which the control rules carry
/// and the slim rules state once in the VCS section.
pub(crate) fn origin_section(cube: &str, push_only_bullet: bool) -> String {
    let mut section = ORIGIN_HEAD.to_owned();
    if push_only_bullet {
        section.push_str(&origin_push_only_bullet(cube));
    }
    section.push_str(ORIGIN_TAIL);
    section
}

/// Merge-conflict telemetry, reuse, boundaries, screenshot and probes sections.
pub(crate) fn telemetry_through_probes(boss: &str, cube: &str) -> String {
    format!(
        "## Merge-conflict telemetry\n\
         \n\
         If you ever run `{cube} workspace rebase` mid-task (e.g. to sync with\n\
         `main` before pushing, or because a push was rejected) and it reports\n\
         `REBASED_WITH_CONFLICTS`, resolve the conflicts as normal, then\n\
         AFTER resolving run:\n\
         \n\
         ```sh\n\
         {boss} engine conflicts record-producer --execution-id <your execution id> \\\n\
           --head-branch <branch> --base-branch <main_branch> --files <comma-separated conflicted paths>\n\
         ```\n\
         \n\
         using the `branch`, `main_branch`, and conflicted-file list `cube\n\
         workspace rebase` printed (or `jj resolve --list` if you need it\n\
         again). This is telemetry only — best-effort, never blocks or\n\
         reverts your actual work — but it is the only way the engine learns\n\
         about conflicts you resolve on your own, before they'd ever reach\n\
         the reviewed in-PR conflict-resolution flow.\n\
         \n\
         ## Reuse before you build\n\
         \n\
         Before implementing any cross-cutting capability (an API/HTTP client,\n\
         config loading, caching, logging wiring, retry/backoff, or similar\n\
         infrastructure), search the repo for an existing implementation\n\
         first. If one exists, reuse or extend it instead of writing a new\n\
         one. If duplication is genuinely necessary, say so explicitly in the\n\
         PR description with the reason — a justified, visible\n\
         exception is the only way to avoid a \"revision required\"\n\
         reuse/duplication finding from the automated reviewer.\n\
         \n\
         ## Boundaries\n\
         \n\
         - Do not modify files outside this workspace. Sibling workspaces\n\
           belong to other workers.\n\
         - Do not modify cube's database, lease state, or workspace registry.\n\
         - `~/Library/Application Support/Boss/` is engine-only.\n\
           Never read, write, or touch it. Do not query the database\n\
           yourself; ask for any task or product context you need.\n\
           `bossctl` is not available to workers.\n\
         \n\
         ## Capturing a screenshot for your own verification\n\
         \n\
         **Do not commit capture PNGs** — and do not delete them either.\n\
         Attach them:\n\
         \n\
         ```sh\n\
         {boss} attach /tmp/shot.png --caption \"wide table, after the fix\"\n\
         ```\n\
         \n\
         The engine stores the image outside this workspace (which is\n\
         recycled after your run), for your own verification and for\n\
         someone looking at this run locally. `{boss} attach --list` shows\n\
         what this task already has, across runs.\n\
         \n\
         Validation is immediate and typed: a path outside this workspace or\n\
         a temp dir, a file that is not really a PNG/JPEG, an oversize render,\n\
         or a per-run cap all come back as an error you can fix and retry now,\n\
         rather than a silent failure discovered after you are gone. Nothing\n\
         is ever downscaled or truncated behind your back — an image is either\n\
         stored exactly as you rendered it or refused.\n\
         \n\
         **Do not put an attachment URL — or any other localhost link — in a\n\
         PR body.** The link it prints is only reachable on this machine;\n\
         nobody reading the PR on GitHub can open it. This applies whether or\n\
         not the evidence server is listening: if it isn't, the image is\n\
         still stored, but that is still not a reason to paste a URL or inline\n\
         it as base64. Summarize registered screenshots by caption in the evidence\n\
         section; that notice does not give GitHub readers access to the images. Where\n\
         a PR body needs evidence a GitHub reader can actually open, use\n\
         something that is — a CI artifact, a deploy preview, a code\n\
         reference.\n\
         \n\
         ## Probes\n\
         \n\
         Messages may arrive between turns. Treat them as questions from a\n\
         human reviewer — short, specific answers.\n"
    )
}

/// The `--draft` PR creation directive, when the run is in draft-PR mode.
pub(crate) fn draft_directive(draft_pr_mode: bool, cube: &str) -> String {
    if !draft_pr_mode {
        return String::new();
    }
    format!(
        "\n## PR creation mode\n\
         \n\
         Default PR creation mode: pass `--draft` to `{cube} pr create`\n\
         unless the task description explicitly says to create a non-draft PR.\n"
    )
}

/// The revision-rules sentence about what to do when blocked.
pub(crate) fn revision_blocked_sentence(boss: &str, worker_signal_seam: bool) -> String {
    if worker_signal_seam {
        format!(
            "If a guard, policy, or decision blocks progress, use `{boss} propose blocked --reason \"<specific question>\"` instead of declaring you could not complete."
        )
    } else {
        "If a guard, policy, or decision blocks progress, emit a `[blocked] reason=\"<specific question>\"` marker instead of declaring you could not complete.".to_owned()
    }
}
