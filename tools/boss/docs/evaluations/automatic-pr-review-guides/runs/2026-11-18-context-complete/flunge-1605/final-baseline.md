## Orientation

This PR fixes an account-level inconsistency: **two people tracking the same fencer could see different British standings because only one had entered the licence number.** Previously, the number lived inside each user’s `trackedFencers` entry, and standings used that viewer’s private copy.

The new contract is one shared licence per fencer. Any account tracking that fencer can set, correct, or clear it; the last write wins. Tracking still controls access to British standings.

This guide compares `9806138` with `256630b`. Validation results below are attributed to the PR description; I did not rerun tests.

## 1. Core fix: separate the shared value from permission to use it

The decisive change is in [the licence resolver](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/view/standings/fencer.rs:754).

Previously, matching the tracked fencer also supplied the value:

```rust
tracked
  .iter()
  .find(|tracked| &tracked.fencer_id == fencer_id)
  .and_then(|tracked| tracked.british_fencing_licence.clone())
```

Now, tracking is only the permission check. After that check, the resolver reads shared storage:

```rust
if !may_resolve_british_licence(tracked, fencer_id) {
  return None;
}
match crate::fencer_licence::get_membership_id(fencer_id).await {
  Ok(licence) => licence,
  // Load errors are logged and return None.
```

That storage is a new Cosmos collection, `fencer_british_licences`, accessed through the new `fencer_licence` crate. [Its model](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/fencer_licence/src/model.rs:10) contains:

```rust
pub struct FencerLicenceFields {
  pub fencer_id: TypedId<FencerId>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub membership_id: Option<String>,
  pub set_by: TypedId<UserId>,
  pub set_at: DateTime<Utc>,
}
```

[The repository](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/fencer_licence/src/repository.rs:17) gives each record the stable ID `bfl-{fencerId}` and performs an ordinary upsert. Clearing preserves the record and its latest writer/time while omitting `membershipId`. This is provenance for the latest write, not a history of previous values.

The separate collection matters: scraper refreshes mutate `FencerFields` in the fencers collection. They cannot overwrite this licence through that existing refresh path. A licence can also exist for a tracked `f.u.*` ID without requiring a corresponding stored fencer profile.

## 2. Worked example: one parent enters the number

**Illustrative scenario**, using the licence and fencer-ID formats present in the tests:

- Alice and Bob both track `f.u.100344622`.
- Neither initially has a licence configured.
- Alice submits:

```http
PUT /api/v3/user/tracked-fencers/f.u.100344622/british-fencing-membership

{"membershipId":" 142013 "}
```

- British rankings are enabled, and the published licence index/table contains `142013`. Assume its Cadet Women’s Sabre row has rank 12 and 125 points, matching values used in the existing British test helpers.

**Before this PR:**

1. The endpoint verifies that Alice tracks the fencer and trims the input to `"142013"`.
2. The write helper patches **Alice’s user document**.
3. Bob’s standings request finds his matching tracked-fencer entry, whose licence remains `None`.
4. `get_fencer_standings(..., None)` skips British assembly. Alice gets the British standing; Bob does not. Bob’s settings response also lacks the licence.

**After this PR:**

1. The same endpoint performs the same tracking and normalization checks.
2. [The write helper](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/v2/user.rs:249) upserts `bfl-f.u.100344622`, with membership `"142013"`, `setBy: "u.d.alice"`, and the current timestamp. It no longer patches Alice’s user document.
3. Bob’s next standings request passes the tracking gate even though his stored entry contains no licence.
4. The resolver loads `"142013"` from the shared record.
5. [The existing standings assembly](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/view/standings/fencer.rs:800) calls `british_standing_items("142013")`. The licence index locates the table; row selection verifies the licence; the row becomes a British standing with key `s.d.gb-CDTWS`, rank 12, and 125 points.
6. Bob’s next `/user/me` response also contains `britishFencingLicence: "142013"`.

**Observable result:** both parents receive the same British standing and settings value after fetching again. The PR adds no live browser notification mechanism.

The useful boundary is an unrelated viewer: a signed-in account that does not track this fencer fails the gate, while an ordinary signed-out request passes no licence at all. Neither receives British items merely because a shared record exists.

## 3. Major logic and architecture changes

**Write path: existing API, new persistence destination.**  
[The v3 handler](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/v3/user.rs:81) retains its existing behavior: malformed fencer IDs and overlong values produce 400; an untracked fencer produces 404; successful writes return 204. Null, empty, or whitespace-only input clears the licence; other input is trimmed and limited to 32 characters.

The substantive change sits underneath it: the helper checks the user’s tracked fencers and calls `set_licence`, which supplies the writer and timestamp to the shared repository. Consequently, another tracker’s correction or clear affects everyone using that shared record.

**Profile reads preserve the frontend contract.**  
[The new blob facade](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/fencer_licence.rs:69) collects all tracked fencer IDs and fetches their licences with one filtered query using `cosmos::id_in`. It then overwrites the response fields:

```rust
for tracked in &mut user.fields.tracked_fencers {
  tracked.british_fencing_licence =
    by_fencer.get(&tracked.fencer_id).cloned().flatten();
}
```

Both GET `/user/me` and the generic profile PATCH response invoke this hydration in [app.rs](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/app.rs:808). It does not persist the hydrated values. A collection-read failure yields unset licence fields while leaving the rest of the profile available; stale per-user values are not used as fallback.

The settings editor therefore needs no new endpoint or component change. Its existing save callback already refreshes the authenticated user.

**Standings, composition, shares, and history use the shared resolver.**  
The resolver becomes asynchronous, and its callers now await it:

- Normal standings and composition use the signed-in caller’s tracked-fencer list.
- Share standings and composition use the link owner’s list and the link’s bound fencer.
- The history handler resolves the shared licence before assembling the current standing.

The existing British rankings flag and table-matching logic remain in effect. A configured licence alone does not guarantee a British item: the enabled source must contain a matching row.

**Capture no longer arbitrates competing account assertions.**  
[Connected-fencer enumeration](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/v2/user.rs:816) now combines connected USA Fencing IDs with shared licence documents, excluding unset licences and unconnected fencers.

[Capture](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/standings_history/capture.rs:63) turns those pairs directly into `HashMap<u32, String>`. It removes the former grouping, deduplication, and `BritishLicenceResolution::Ambiguous` branch. The corresponding `britishLicenceAmbiguous` counter disappears from backend state, TypeScript types, and the Admin status display.

**Migration is explicit operational work.**  
The registered `migrate_british_fencing_licences` admin task invokes [migration orchestration](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/fencer_licence.rs:155). It is not scheduled at startup.

| Historical state for a fencer                     | Migration behavior                                                                             |
| ------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| One distinct asserted licence                     | Adopt it; choose the lexicographically smallest asserting user ID for provenance.              |
| Multiple distinct licences                        | Write no new shared value; preserve the competing per-user fields and log the fencer ID/count. |
| An adoption candidate already has a shared record | Skip adoption, including when that record represents an explicit clear.                        |

Afterward, it strips legacy fields except for conflicted fencers and invalidates affected user caches. The planner is pure; database scanning, writing, and cleanup live in the blob facade. Existing legacy values therefore need this admin task—or a new explicit user write—to become visible through the shared read path.

There is also a new [generic PATCH guard](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/v2/user.rs:469) rejecting operation paths containing `britishFencingLicence`, independently of the tracked-fencer lockdown flag. Review its precise scope: it checks destination path strings, not embedded values in a replacement of a parent object.

## 4. Tests and supporting infrastructure

The added coverage concentrates on pure rules and structural boundaries:

| Area                                                                                                                                         | Added or changed coverage                                                                                                                                                   |
| -------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [Migration planner](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/fencer_licence/src/migrate.rs:116)               | Six tests cover empty input, one assertion, agreeing users, deterministic attribution, conflicting values, independent fencers, and conflict counts without licence values. |
| Licence model and keys                                                                                                                       | Two serialization tests cover present/cleared membership values; two repository tests cover stable, distinct document IDs.                                                  |
| [Blob migration helpers](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/fencer_licence.rs:236)             | Five tests cover assertion collection, cleanup, no-op cleanup, and retaining conflicting values.                                                                            |
| [Scraper isolation](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/fencingtracker/mod.rs:833)              | Two tests assert that `FencerFields` has no licence key and that a simulated refresh emits no licence patch.                                                                |
| Generic PATCH                                                                                                                                | One new test rejects a direct licence-field replacement with lockdown disabled.                                                                                             |
| [Tracking gate](/private/tmp/boss-review-guide-corpus-793/checkouts/flunge-1605/backend/blob/src/view/standings/fencer_british_tests.rs:540) | Three existing resolver tests become permission tests, including a tracker who never entered a number.                                                                      |
| Capture                                                                                                                                      | Eight obsolete grouping/ambiguity tests are removed; summary and serialization tests are updated for the removed counter.                                                   |

Infrastructure changes add the `//backend/fencer_licence:fencer_licence_test` Bazel target, register the crate in Cargo, connect it to `blob`, and update lockfiles. Tests reuse existing fencer fixtures and add small local user/assertion builders; there is no new database emulator or integration harness.

**Coverage limit:** the new helper tests do not exercise `hydrate_user`, actual Cosmos upserts, the full migration, or Alice-writing/Bob-reading through HTTP. The PR description’s reference to hydration-orchestration tests is broader than the tests present in the diff.

The PR description reports passing backend tests, the backend build, repository checks, and the frontend TypeScript/Vite build. It supplies no production migration counts or registered screenshots.

https://github.com/brianduff/flunge/pull/1605
