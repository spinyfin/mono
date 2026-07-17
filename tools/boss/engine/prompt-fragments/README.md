# boss-engine-prompt-fragments

Holds agent-prompt text that more than one prompt renderer emits
verbatim. Today that is a single fragment: the `## Boundaries` +
`## Coordinator` block shared by the `pr_review` reviewer prompt (in
`boss-engine-pr-review`) and the `automation_triage` prompt (in
`boss-engine`).

## Architecture

The crate holds static strings and nothing else — no deps, no I/O, no
types. It exists for a dependency reason rather than a functional one:
its two consumers live in different crates, and the engine depends on
the pr-review crate, so a copy in either consumer would either duplicate
the wording or invert that edge. Sitting below both keeps the engine ->
pr-review edge one-directional and keeps a wording change a one-line
edit in one place.

Shared-by-accident wording does not belong here. Renderers whose
boundaries text differs _on purpose_ keep their own copy — see the
crate-level docs in `src/lib.rs` for which ones and why. Add a fragment
here only when the sites are meant to stay identical.
