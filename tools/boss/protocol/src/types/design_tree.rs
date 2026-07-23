//! The markdown document tree the Designs tab browses, read from
//! GitHub at HEAD of a product's configured repo.
//!
//! GitHub is the source of truth: a document is identified by the
//! `(repo_remote_url, path, git_ref)` triple carried on
//! [`DesignDocTree`] + [`DesignDocEntry`], and Boss never mirrors
//! document bodies into its own state as the canonical copy. The
//! engine may cache a *listing* (see the design-docs service) because
//! it can be invalidated against HEAD; bodies are always read through.

use serde::{Deserialize, Serialize};

/// One markdown file in a product repo's tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DesignDocEntry {
    /// Repo-relative path, e.g. `docs/design-docs/foo.md`. The UI
    /// nests these into a directory tree; the engine deliberately
    /// sends the flat path list rather than a pre-nested structure so
    /// the wire shape stays trivially diffable and the nesting rule
    /// lives in one place on the client.
    pub path: String,
    /// Blob size in bytes, when GitHub reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// A product repo's markdown listing at one commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct DesignDocTree {
    /// The product's configured repo remote, echoed back so the client
    /// can pass it to `GetProductDesignDoc` without re-deriving it.
    pub repo_remote_url: String,
    /// `owner/repo`, for display and for building GitHub web links.
    pub owner_repo: String,
    /// The repo's default branch, whose tip this listing was read at.
    pub branch: String,
    /// The resolved commit sha. This — not the branch name — is the
    /// `ref` a client passes back when opening a document, so a push
    /// that lands mid-browse cannot change what a click opens.
    pub git_ref: String,
    /// Markdown files at `git_ref`, sorted by path.
    pub entries: Vec<DesignDocEntry>,
    /// RFC 3339 timestamp of when this listing was read from GitHub.
    /// Lets the UI show the listing's age rather than implying it is
    /// live.
    pub fetched_at: String,
    /// GitHub reported the underlying tree as truncated (a repo too
    /// large for one recursive response). The listing is a subset;
    /// surfaced so the UI can say so instead of silently showing
    /// partial results.
    #[serde(default)]
    #[builder(default)]
    pub truncated: bool,
}

/// What the engine could determine about a product's markdown tree.
///
/// The four non-`Loaded` variants are the four failure modes that need
/// distinct, actionable messages in the UI. They are deliberately
/// separate variants rather than one `Error { reason }`: the remedy
/// differs in each case (configure a repo / fix access / write a doc /
/// wait), and collapsing them is how the tab ended up showing a bare
/// "not found" for every condition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DesignDocTreeState {
    /// The product has no `repo_remote_url`. Remedy: set one on the
    /// product.
    NoRepoConfigured,
    /// A repo is configured but GitHub could not be reached, the repo
    /// does not exist, or the credential cannot see it. Remedy:
    /// check the URL / `gh auth status`.
    Unreachable {
        repo_remote_url: String,
        /// GitHub's (or `gh`'s) own message, shown under the headline
        /// so the operator sees what actually failed.
        reason: String,
    },
    /// GitHub refused the read because we are rate-limited. Distinct
    /// from `Unreachable` because retrying immediately will fail the
    /// same way — the remedy is to wait, not to re-check the config.
    RateLimited { repo_remote_url: String, reason: String },
    /// The repo was read successfully and genuinely contains no
    /// markdown files at HEAD. Remedy: write one — nothing is broken.
    Empty {
        repo_remote_url: String,
        owner_repo: String,
        git_ref: String,
    },
    /// The listing resolved.
    Loaded { tree: DesignDocTree },
}

/// The result of reading one document's body from GitHub.
///
/// Modelled as a two-variant enum carried on the success event rather
/// than as a generic work-error, so a failed document open renders
/// inline in the reader pane instead of as a global error banner
/// detached from the document it refers to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DesignDocContent {
    Loaded { markdown: String },
    Failed { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `type`-tagged representation is what the Swift client
    /// decodes against, so pin the discriminator spelling — a rename
    /// here silently breaks the app with no Rust-side failure.
    #[test]
    fn tree_state_variants_are_snake_case_type_tagged() {
        let raw = serde_json::to_value(DesignDocTreeState::NoRepoConfigured).unwrap();
        assert_eq!(raw["type"], "no_repo_configured");

        let raw = serde_json::to_value(DesignDocTreeState::RateLimited {
            repo_remote_url: "git@github.com:foo/bar.git".to_owned(),
            reason: "API rate limit exceeded".to_owned(),
        })
        .unwrap();
        assert_eq!(raw["type"], "rate_limited");
        assert_eq!(raw["reason"], "API rate limit exceeded");
    }

    #[test]
    fn loaded_tree_round_trips() {
        let state = DesignDocTreeState::Loaded {
            tree: DesignDocTree::builder()
                .repo_remote_url("git@github.com:brianduff/flunge.git")
                .owner_repo("brianduff/flunge")
                .branch("main")
                .git_ref("b95bd654ec91f84f70f62127ef8d53317bd52ebb")
                .entries(vec![DesignDocEntry {
                    path: "docs/design-docs/backend-preview-environments.md".to_owned(),
                    size: Some(4096),
                }])
                .fetched_at("2026-07-23T12:00:00Z")
                .build(),
        };
        let raw = serde_json::to_value(&state).unwrap();
        assert_eq!(raw["type"], "loaded");
        // `truncated` defaults to false and must survive the round trip.
        let back: DesignDocTreeState = serde_json::from_value(raw).unwrap();
        assert_eq!(back, state);
    }

    /// An older engine (or a hand-written fixture) omitting `truncated`
    /// must still decode — it defaults to "not truncated".
    #[test]
    fn tree_decodes_without_truncated_field() {
        let raw = serde_json::json!({
            "repo_remote_url": "git@github.com:foo/bar.git",
            "owner_repo": "foo/bar",
            "branch": "main",
            "git_ref": "abc123",
            "entries": [],
            "fetched_at": "2026-07-23T12:00:00Z",
        });
        let tree: DesignDocTree = serde_json::from_value(raw).unwrap();
        assert!(!tree.truncated);
    }

    #[test]
    fn doc_content_variants_are_type_tagged() {
        let raw = serde_json::to_value(DesignDocContent::Loaded {
            markdown: "# Title".to_owned(),
        })
        .unwrap();
        assert_eq!(raw["type"], "loaded");
        assert_eq!(raw["markdown"], "# Title");

        let raw = serde_json::to_value(DesignDocContent::Failed {
            reason: "Not Found".to_owned(),
        })
        .unwrap();
        assert_eq!(raw["type"], "failed");
    }

    /// `size` is optional on the wire: GitHub omits it for some entry
    /// shapes, and an entry without one is still perfectly openable.
    #[test]
    fn entry_decodes_without_size() {
        let entry: DesignDocEntry = serde_json::from_value(serde_json::json!({ "path": "a.md" })).unwrap();
        assert_eq!(entry.size, None);
        // …and it is omitted rather than serialised as null on the way out.
        let raw = serde_json::to_value(&entry).unwrap();
        assert!(raw.get("size").is_none());
    }
}
