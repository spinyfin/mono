use boss_protocol::BranchNaming;

/// Default worker branch-name prefix when the product's branch-naming
/// strategy is [`BranchNaming::BossExecPrefix`]. Preserves the
/// historical `boss/exec_<id>` shape so existing setups are unchanged.
pub const DEFAULT_WORKER_BRANCH_PREFIX: &str = "boss/";

/// Engine-supplied branch name a worker must push to when opening
/// the PR for an execution. The exact shape depends on the execution's
/// [`BranchNaming`] strategy (snapshotted from the product's
/// `editorial_rules.branch_naming` at spawn time) and on the execution's
/// frozen `worker_branch_prefix` (snapshotted from the product's
/// `Product::worker_branch_prefix` column):
///
/// - [`BranchNaming::BossExecPrefix`] (default): `<prefix><execution_id>`,
///   where `<prefix>` is `worker_branch_prefix` when the product set one
///   (e.g. `bduff/` → `bduff/exec_<id>`) and `boss/` otherwise. This is the
///   knob exposed by `boss product … --worker-branch-prefix`; the execution
///   id is kept verbatim so the branch is unique per execution by
///   construction.
/// - [`BranchNaming::OpaqueHash`]: `boss/<sha256(execution_id)[..8]>` —
///   omits the execution id from the branch name while remaining unique
///   within a repo (32 bits of hash space).
/// - [`BranchNaming::CustomPrefix`]: `<prefix>/<sha256(execution_id)[..8]>` —
///   user-supplied prefix instead of `boss/`, same opaque hash suffix.
///
/// `worker_branch_prefix` only affects the default `BossExecPrefix`
/// strategy: a non-default `branch_naming` is the richer, explicitly
/// configured editorial rule and takes precedence over the plain prefix
/// column. The two knobs also differ in slash convention —
/// `worker_branch_prefix` already carries its trailing `/` (it is
/// concatenated verbatim), whereas `CustomPrefix { prefix }` inserts a `/`.
///
/// In every strategy the branch name is derived deterministically from
/// `execution_id` (and the frozen prefix) so the detector can reconstruct
/// it from `state.db` alone — no local jj reads, no shared-store
/// contamination.
///
/// See `tools/boss/docs/postmortems/incident-001-pr-fan-out.md` §5 for
/// the uniqueness rationale. Cross-repo hash collisions (R6) are not
/// collisions: the `gh pr list --head` query is always scoped to the
/// product's `repo_remote_url`.
pub fn expected_branch_name(
    execution_id: &str,
    branch_naming: &BranchNaming,
    worker_branch_prefix: Option<&str>,
) -> String {
    match branch_naming {
        BranchNaming::BossExecPrefix => {
            let prefix = worker_branch_prefix.unwrap_or(DEFAULT_WORKER_BRANCH_PREFIX);
            format!("{prefix}{execution_id}")
        }
        BranchNaming::OpaqueHash => {
            let hash = opaque_hash(execution_id);
            format!("boss/{hash}")
        }
        BranchNaming::CustomPrefix { prefix } => {
            let hash = opaque_hash(execution_id);
            format!("{prefix}/{hash}")
        }
    }
}

/// First 8 hex characters of the SHA-256 digest of `execution_id`.
/// Used by [`BranchNaming::OpaqueHash`] and [`BranchNaming::CustomPrefix`]
/// to build a short, unique-by-construction branch suffix that does not
/// leak the literal execution id into the branch name.
fn opaque_hash(execution_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(execution_id.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// The work-item-identifying suffix of a branch name: everything after
/// the final `/` (or the whole string when there is no `/`).
///
/// Every engine-supplied branch name has the shape
/// `<prefix>/<work-item-suffix>` (see [`expected_branch_name`]), where the
/// suffix is what uniquely binds the branch to one execution — the
/// `exec_<id>` under [`BranchNaming::BossExecPrefix`], or the opaque hash
/// under [`BranchNaming::OpaqueHash`] / [`BranchNaming::CustomPrefix`].
/// The prefix (`boss/`, `bduff/`, …) is cosmetic and product-configurable
/// (`worker_branch_prefix`), so PR↔work-item association keys on the
/// suffix, never on the prefix.
pub fn branch_work_item_suffix(branch: &str) -> &str {
    branch.rsplit('/').next().unwrap_or(branch)
}

/// Whether two branch names identify the same work item, **ignoring their
/// prefixes**. A worker that honours a product's `worker_branch_prefix`
/// (e.g. `bduff/`) opens its PR on `bduff/<suffix>` while the engine
/// reconstructs `boss/<suffix>` as the expected branch; those must
/// associate so the worker is not forced to abandon a compliant PR and
/// recreate it under `boss/` (see issue #1145).
///
/// The work-item suffix is unique per execution within a repo (it is the
/// execution id or a hash of it), so matching on the suffix alone is just
/// as safe against cross-execution mis-binding as the exact-branch match
/// it replaces — a sibling worker's branch cannot share this execution's
/// suffix. An empty suffix never matches (defensive: a malformed `…/`
/// branch must not collide with another).
pub fn branches_identify_same_work_item(a: &str, b: &str) -> bool {
    let suffix_a = branch_work_item_suffix(a);
    !suffix_a.is_empty() && suffix_a == branch_work_item_suffix(b)
}

/// Whether `head` identifies any of `branches` ([`branches_identify_same_work_item`]).
/// A work item's own PR is on the branch of ANY of its primary implementation
/// executions (a resumed execution pushes to its predecessor's branch), so
/// ownership is decided against the whole set, never the latest execution.
pub fn head_identifies_any_branch(head: &str, branches: &[String]) -> bool {
    branches.iter().any(|b| branches_identify_same_work_item(head, b))
}
