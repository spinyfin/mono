# Decisions for long-conflicting PRs

Before automatically resolving another conflict, Boss checks the PR's lifetime history. It stops after **three completed resolutions or seven days since the first recorded in-review conflict**, whichever comes first. Producer-side and speculative observations do not count. A resolution already running is allowed to finish.

Set `BOSS_CONFLICT_MAX_RESOLUTIONS` and `BOSS_CONFLICT_MAX_DAYS` on the engine to override these positive integer limits. Invalid values refuse automatic conflict work and report an error in the engine log. The age measures recorded conflict history, not the PR's creation date.

The defaults are policy choices informed by [the conflict investigation](https://github.com/spinyfin/mono/pull/3558): merged PRs averaged 1.8 conflict states, 96 of 108 open PRs were over a week old, and their median age was 68 days. Three resolutions leaves room for ordinary recurrence while seven days catches standing inventory before months of repeated repairs. The investigation did not measure an optimal cutoff.

Stopped PRs remain blocked by their merge conflict. Each gets an abandoned ledger row with `failure_reason=lifetime_conflict_limit`; completed resolutions retain their original outcomes. Questions join one open batch per product in Attentions. Answer each PR, skip any that should remain stopped, and apply the batch:

- **Merge now** creates one final conflict-resolution revision. Once it succeeds and remaining revisions finish, Boss requests the product's normal checked merge, including Trunk when configured. Required checks and reviews still apply.
- **Close** authorizes closing that PR without merging. Boss waits for live writers to finish, then closes it. It does not delete the branch. The regular poller observes the closed PR and reconciles its task.
- **Rewrite onto the new base** uses the existing large port revision path on the same PR, preserving its behaviour and earlier review fixes. It does not request a merge.

Skipping or dismissing a question leaves the stop in force. The retry-conflict command cannot bypass an unanswered decision. An answer authorizes one resolution or port; another conflict after it completes is evaluated again against the lifetime limits. Stops, answers, and pending external actions survive engine restarts. Failed merge or close requests remain pending with a ledger diagnosis and an attention item; they are never reported as a successful resolution.
