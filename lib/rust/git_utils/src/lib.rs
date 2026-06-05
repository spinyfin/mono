//! Shared git / GitHub plumbing used across cube and the boss engine.
//!
//! This crate is the single home for the general, previously-duplicated
//! git/GitHub utilities that cube and the boss engine each used to carry
//! their own diverged copy of:
//!
//! - [`repo_slug`] — parsing git remote URLs (github.com, GitHub
//!   Enterprise `org-NNN@github.com:owner/repo.git` SSO remotes, HTTPS,
//!   `ssh://`, and SCP shapes) into owner/repo slugs, plus the bare-slug
//!   predicates used to distinguish a cube reponame from a clone URL.
//! - [`gh_cli`] — small `gh`-CLI helpers for fetching PR head metadata.
//! - [`pr_bookmark`] — the reserved local-only `pr/<n>` bookmark
//!   namespace helpers.
//!
//! It intentionally does NOT host the `boss shake` GitHub-App auth code
//! (JWT signing, embedded credentials, issue filing) — that is
//! shake-specific and remains in `boss-github`.

pub mod gh_cli;
pub mod pr_bookmark;
pub mod repo_slug;
