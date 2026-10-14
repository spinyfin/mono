//! Per-product coordinator guidance: the `BOSS_COORDINATOR.md` a product
//! repo may carry at its root, read from GitHub at the default branch's
//! HEAD and pinned to the commit sha that HEAD resolved to.
//!
//! This is coordinator-facing guidance only. It is distinct from
//! `AGENTS.md`, which workers read — coordinator vocabulary must not
//! leak into every worker's context, so coordinator rules live in this
//! separate file and the engine hands them only to the coordinator
//! session. See `tools/boss/docs/coordinator-product-guidance.md`.

use serde::{Deserialize, Serialize};

/// Repo-relative path of the per-product coordinator guidance file.
pub const COORDINATOR_GUIDANCE_PATH: &str = "BOSS_COORDINATOR.md";

/// What the engine could determine about one product's guidance file.
///
/// Every outcome is explicit on purpose: a missing file, an over-cap
/// file, and a fetch failure are three different things with three
/// different remedies, and none of them may read as "no guidance".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CoordinatorGuidanceState {
    /// The file exists at `git_ref` and fits under the cap; `markdown`
    /// is its full text.
    Loaded {
        /// The commit sha the default branch resolved to when read. This
        /// is the version the coordinator is acting on.
        git_ref: String,
        bytes: u64,
        markdown: String,
    },
    /// The repo was reachable at `git_ref` but has no file at the
    /// guidance path. A legitimate state for a product without
    /// coordinator rules, but it is stated, never inferred from an error.
    Missing { git_ref: String },
    /// The file exists at `git_ref` but is larger than the engine's cap.
    /// Its text is withheld; the remedy is to trim the file.
    OverCap {
        git_ref: String,
        bytes: u64,
        cap_bytes: u64,
    },
    /// GitHub could not be read (unreachable, not authorized, rate
    /// limited, repo not found, or the launch-time budget ran out).
    /// `reason` says which. Nothing about the file is known.
    Failed { reason: String },
    /// GitHub could not be read, so this is the engine's last-known-good
    /// result for the product instead: what an earlier successful read
    /// saw at `git_ref`. It is never fresh and never passed off as such;
    /// `fetch_error` is the failure that caused the fallback and
    /// `read_at` says how stale the copy is.
    Cached {
        /// The commit sha the cached read resolved to.
        git_ref: String,
        /// RFC 3339 timestamp of the successful read the copy came from.
        read_at: String,
        /// Why the fresh read failed (the [`Self::Failed`] reason).
        fetch_error: String,
        /// The cached file text, or `None` when the cached read found no
        /// file at `git_ref` (a cached [`Self::Missing`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        markdown: Option<String>,
    },
    /// The product has no `repo_remote_url`, so there is nowhere to read
    /// guidance from.
    NoRepoConfigured,
    /// The product's remote is not a github.com URL, so it cannot be
    /// read through the engine's GitHub path.
    NotGitHub { repo_remote_url: String },
}

impl CoordinatorGuidanceState {
    /// Stable snake_case tag, for audit payloads and log lines.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Loaded { .. } => "loaded",
            Self::Missing { .. } => "missing",
            Self::OverCap { .. } => "over_cap",
            Self::Failed { .. } => "failed",
            Self::Cached { .. } => "cached",
            Self::NoRepoConfigured => "no_repo_configured",
            Self::NotGitHub { .. } => "not_github",
        }
    }

    /// The commit sha the state was resolved at, when the repo was
    /// reachable.
    pub fn git_ref(&self) -> Option<&str> {
        match self {
            Self::Loaded { git_ref, .. }
            | Self::Missing { git_ref }
            | Self::OverCap { git_ref, .. }
            | Self::Cached { git_ref, .. } => Some(git_ref),
            Self::Failed { .. } | Self::NoRepoConfigured | Self::NotGitHub { .. } => None,
        }
    }
}

/// One product's coordinator guidance as the engine reports it, both in
/// the session-start brief and from `boss guidance show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
#[builder(on(String, into))]
pub struct CoordinatorGuidanceView {
    pub product_id: String,
    /// RFC 3339 timestamp of when the engine resolved this state.
    pub fetched_at: String,
    /// Repo-relative path that was read ([`COORDINATOR_GUIDANCE_PATH`]).
    pub path: String,
    pub product_name: String,
    pub state: CoordinatorGuidanceState,
    /// `owner/repo`, when the product's remote parsed as a GitHub URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_repo: Option<String>,
    /// The product's configured remote, echoed back for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote_url: Option<String>,
}

impl CoordinatorGuidanceView {
    /// The guidance text to show, if this view has any: the freshly
    /// loaded file, or a cached copy of it. Callers render it inside
    /// begin/end fences after [`Self::describe_state`], which says
    /// whether it is cached.
    pub fn body(&self) -> Option<&str> {
        match &self.state {
            CoordinatorGuidanceState::Loaded { markdown, .. } => Some(markdown),
            CoordinatorGuidanceState::Cached { markdown, .. } => markdown.as_deref(),
            _ => None,
        }
    }

    /// One-line, operator-readable statement of this product's guidance
    /// state. Shared by the coordinator's session-start brief and by
    /// `boss guidance show`, so the two never describe a state differently.
    pub fn describe_state(&self) -> String {
        let repo = self.owner_repo.as_deref().unwrap_or("(no GitHub repo)");
        match &self.state {
            CoordinatorGuidanceState::Loaded { git_ref, bytes, .. } => {
                format!("LOADED from {repo} @ {git_ref} ({bytes} bytes)")
            }
            CoordinatorGuidanceState::Missing { git_ref } => format!(
                "MISSING: {repo} @ {git_ref} has no `{}`. This product has no coordinator guidance file; if a durable \
                 product-specific lesson comes up, file a chore against this product to add one.",
                self.path
            ),
            CoordinatorGuidanceState::OverCap {
                git_ref,
                bytes,
                cap_bytes,
            } => format!(
                "OVER CAP: `{}` in {repo} @ {git_ref} is {bytes} bytes; the engine caps it at {cap_bytes}. Its text was \
                 NOT loaded. Tell the operator, and file a chore against this product to trim it.",
                self.path
            ),
            CoordinatorGuidanceState::Failed { reason } => format!(
                "FETCH FAILED: `{}` could not be read ({reason}). Nothing is known about this product's guidance — do \
                 NOT treat this as \"no guidance\". Say so to the operator and retry with `boss guidance show --product \
                 {}`.",
                self.path, self.product_id
            ),
            CoordinatorGuidanceState::Cached {
                git_ref,
                read_at,
                fetch_error,
                markdown,
            } => {
                let what = match markdown {
                    Some(text) => format!(
                        "the last-known-good `{}` from {repo} @ {git_ref} ({} bytes)",
                        self.path,
                        text.len()
                    ),
                    None => format!("the last-known-good result: {repo} @ {git_ref} had no `{}`", self.path),
                };
                format!(
                    "CACHED (NOT freshly loaded): GitHub could not be read just now, so this is {what}, as read at \
                     {read_at}. The fetch error was: {fetch_error}. The rules may be newer upstream; retry with `boss \
                     guidance show --product {}`.",
                    self.product_id
                )
            }
            CoordinatorGuidanceState::NoRepoConfigured => {
                "NO REPO: this product has no repo_remote_url, so there is nowhere to read coordinator guidance from."
                    .to_owned()
            }
            CoordinatorGuidanceState::NotGitHub { repo_remote_url } => format!(
                "NOT GITHUB: `{repo_remote_url}` is not a github.com remote, so `{}` cannot be read through the engine's \
                 GitHub path. Nothing is known about this product's guidance.",
                self.path
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cached(markdown: Option<&str>) -> CoordinatorGuidanceView {
        CoordinatorGuidanceView::builder()
            .product_id("prod_boss")
            .fetched_at("2026-10-14T10:00:00Z")
            .path(COORDINATOR_GUIDANCE_PATH)
            .product_name("Boss")
            .state(CoordinatorGuidanceState::Cached {
                git_ref: "abc123".to_owned(),
                read_at: "2026-10-13T09:00:00Z".to_owned(),
                fetch_error: "could not reach GitHub".to_owned(),
                markdown: markdown.map(str::to_owned),
            })
            .owner_repo("spinyfin/mono")
            .build()
    }

    #[test]
    fn cached_state_is_labelled_with_sha_read_time_and_the_fetch_error() {
        let view = cached(Some("# rules\n"));
        let line = view.describe_state();
        assert!(line.starts_with("CACHED (NOT freshly loaded)"), "{line}");
        assert!(line.contains("spinyfin/mono @ abc123"), "{line}");
        assert!(line.contains("2026-10-13T09:00:00Z"), "{line}");
        assert!(line.contains("could not reach GitHub"), "{line}");
        assert!(line.contains("boss guidance show --product prod_boss"), "{line}");
        assert_eq!(view.body(), Some("# rules\n"));
        assert_eq!(view.state.tag(), "cached");
        assert_eq!(view.state.git_ref(), Some("abc123"));
    }

    #[test]
    fn cached_state_round_trips_with_and_without_a_body() {
        for markdown in [Some("# rules\n"), None] {
            let view = cached(markdown);
            let json = serde_json::to_string(&view).unwrap();
            assert!(json.contains("\"state\":\"cached\""), "{json}");
            assert_eq!(serde_json::from_str::<CoordinatorGuidanceView>(&json).unwrap(), view);
        }
    }
}
