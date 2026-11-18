# Shared British Fencing licences: one entry for every account tracking a fencer

## Orientation

Two parents tracking the same child should both see the child’s British rankings after either parent enters the membership number. Previously, the number belonged to the account that entered it, so the other parent could see different standings. In Flunge, “tracking” means adding a fencer to an account’s connected-fencer list; “standings” are the ranking positions and points shown for that fencer. British Fencing calls its membership number a “licence.”

This change makes the licence shared across everyone tracking that fencer. Any tracker can set, correct, or clear it, and the latest write replaces the previous value while recording who made it and when. The work lives in the backend’s licence storage, user-profile responses, and standings readers. British rankings still require an eligible viewer: a signed-in tracker, or someone using a live results-share link owned by a tracker. A results-share link grants access to one bound fencer; the shared licence does not make British standings available through an ordinary signed-out fencer lookup.

## Change overview

The main behavioral change is the ownership of the licence. The [old setter](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/v2/user.rs#L246-L268) changed one user document. The [new setter](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L250-L275) checks that the account tracks the fencer, then writes a shared record.

The existing settings editor and membership endpoint remain the entry points. Historical account-specific numbers are moved by an explicitly invoked administration task. **Deploying the change does not automatically migrate existing numbers:** an operator must run `migrate_british_fencing_licences`. The task is [registered without a schedule](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/admintasks/src/lib.rs#L157-L167), and [startup deliberately omits the full user scan](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/app.rs#L85-L90).

## Core implementation

The new `fencer_licence` Rust crate—a separate backend library—owns the record model, database operations, and migration planning. It accepts a collection handle rather than depending on the main application. The application’s `blob` library supplies that handle and connects the library to requests and administrative jobs.

The record lives in `fencer_british_licences`, a collection in Cosmos, the application’s document database. This is a _sidecar_: a separate record associated with a fencer rather than another field on the automatically refreshed fencer profile. Its [model](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/model.rs#L5-L17) holds the fencer ID, optional membership number, `setBy` (last writer), and `setAt` (write time).

The [repository](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L18-L28) derives a stable key:

```rust
pub fn document_id(fencer_id: &TypedId<FencerId>) -> String {
  format!("bfl-{}", fencer_id)
}
```

Writes use an [upsert](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L79-L94), meaning create the document if absent or replace it if present. Clearing keeps the document and last-writer information, with no membership number. This is one current record, not a history of every edit.

The separation matters because the existing [fencer refresh](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencingtracker/mod.rs#L438-L461) merges downloaded profile data into `FencerFields`, the stored profile structure. The licence is outside that structure and collection.

## Implementation walkthrough

### 1. Route the existing editor to shared storage

The [unchanged settings editor](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/frontend/src/SettingsPage.tsx#L455-L463) submits the membership number and reloads the account profile. Its endpoint remains `PUT /api/v3/user/tracked-fencers/{fencerId}/british-fencing-membership`.

The [request handler](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v3/user.rs#L65-L108) retains its validation: trim surrounding whitespace; treat null, empty, or whitespace-only input as clearing; reject more than 32 characters; return 404 if the fencer is not tracked. The setter checks tracking again using a database-loaded user, then calls the shared write with the account ID. [Application wiring](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L30-L44) supplies the current time. This path no longer patches the user document.

For generic profile edits, the new [patch validation](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L470-L509) rejects an operation whose destination path contains `britishFencingLicence`, independently of the flag that blocks tracked-fencer edits generally. The check is on the operation’s path string.

### 2. Preserve the profile response used by settings

_Hydration_ here means filling a response field from another record without storing that copy on the user. Both [GET and PATCH profile responses](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/app.rs#L808-L834) hydrate `trackedFencers[].britishFencingLicence`.

The [batch reader](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L43-L77) queries all requested record IDs together using the existing `cosmos::id_in` helper; an empty tracked list needs no query. The [hydrator](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L69-L90) overwrites every response entry from that result:

```rust
for tracked in &mut user.fields.tracked_fencers {
  tracked.british_fencing_licence = by_fencer.get(&tracked.fencer_id).cloned().flatten();
}
```

A failed shared-record query produces an empty result map after logging a warning. The profile still returns, with licence fields unset; old account-specific values are not used as fallback.

### 3. Separate viewing eligibility from the source of the number

The decisive [standings resolver](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L754-L786) first checks tracking, then reads shared storage:

```rust
if !may_resolve_british_licence(tracked, fencer_id) {
  return None;
}
match crate::fencer_licence::get_membership_id(fencer_id).await {
  Ok(licence) => licence,
```

The omitted error arm warns and returns `None`. The tracking check reuses [`is_connected_fencer`](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/share/mod.rs#L43-L50), which checks whether the exact fencer ID appears in the supplied list.

[Standings and composition handlers](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L1400-L1430) now await this resolver. A composition is the event-by-event explanation of a points total. The [history handler](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer_history.rs#L133-L151) does the same before finding the requested current standing. Share requests supply the [link owner’s tracked list and bound fencer](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/share/view.rs#L277-L286), including for [composition](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/share/view.rs#L328-L343).

**Worked example — illustrative inputs.** Alice and Bob both track `f.u.100344622`, a USA Fencing-based fencer ID. Neither has entered a number. Alice saves `" 142013 "`. Assume British rankings are enabled and a published table contains that licence.

1. **Before:** the handler trimmed the input, but the old setter stored `142013` only on Alice’s account. The [old resolver](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/view/standings/fencer.rs#L756-L764) read the matching entry on the viewing account. Alice obtained the number; Bob obtained `None`, so his response omitted British standings.
2. **Now:** the same input creates or replaces `bfl-f.u.100344622`, recording Alice and the write time. Bob passes the tracking check even though he never entered a number, and the resolver returns the same `142013` for both accounts.
3. The [existing assembly](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L834-L903) uses that number to retrieve British ranking entries, subject to the runtime flag. Both parents now receive the matching British standing; Bob’s next profile response also contains the number for settings.
4. A signed-out request without a share link still passes no licence to assembly. A shared record alone does not satisfy the viewing check.

### 4. Move historical values without choosing between competing numbers

The [migration planner](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/migrate.rs#L66-L109) groups historical account entries by fencer and then by distinct licence. One distinct number yields an adoption decision, attributed to the alphabetically smallest asserting user ID. Multiple distinct numbers yield a conflict decision containing only the fencer ID and distinct-count.

The [application migration](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L149-L220) loads users and existing shared records. For adoption decisions, it skips fencers found in that existing-record snapshot, including records explicitly cleared. Conflicts produce a warning without the competing numbers and preserve the historical entries; that branch does not clear an existing shared record. Finally, non-conflicting old fields are stripped and affected user caches invalidated. The administration task reports adopted, conflicted, skipped-already-shared, and stripped-user counts.

### 5. Simplify historical capture

_Capture_ is the background job that saves standings snapshots for later history views. Its [input enumeration](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L794-L833) now combines tracked USA Fencing IDs with shared records, ignoring cleared licences and records outside that population.

The job builds [one map from fencer ID to licence](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/standings_history/capture.rs#L63-L77), replacing the former grouping and disagreement-resolution machinery. The ambiguity counter disappears from capture state, its serialized response, and the administrative display. Conflicting historical account entries are now a migration concern rather than a capture-time decision.

## Tests and test infrastructure

**Added tests:**

- [Migration planning](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/migrate.rs#L134-L230): empty input, one assertion, agreement between users with deterministic attribution, independent fencers, and two/three distinct numbers. The conflict test also asserts that debug output excludes the competing numbers.
- [Application migration helpers](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L230-L328): local user/tracked-fencer fixtures exercise assertion collection, ignoring absent values, no-op stripping, removal of ordinary values, and retention of conflicted values while removing another fencer’s value.
- [Model serialization](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/model.rs#L42-L75) checks present and omitted membership numbers; [repository key tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L104-L121) check the key prefix and stability.
- [Refresh isolation](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencingtracker/mod.rs#L821-L888) uses a sample downloaded profile and the existing fencer fixture to assert that profile JSON contains no licence field, refresh-generated patch paths contain no licence names, and the shared key differs from the profile key.
- [Generic patch rejection](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L592-L605) rejects a replacement of the licence field with tracked-fencer lockdown disabled.

**Modified tests:** the [three former account-licence lookup tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer_british_tests.rs#L540-L576) now test tracking eligibility, including a tracker who entered nothing. [Capture-state tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/standings_history/state.rs#L352-L379) assert the revised summary and absence of `britishLicenceAmbiguous` from serialized output.

**Removed tests:** the [old capture tests](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/standings_history/capture.rs#L642-L722) for grouping account assertions, deduplicating agreement, refusing disagreement, and tallying ambiguity disappear with those functions.

**Infrastructure:** the new crate has a [Bazel library and `fencer_licence_test` target](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/BUILD#L1-L21), plus [Cargo dependencies and a JSON serialization test dependency](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/Cargo.toml#L1-L14). Workspace/build dependencies and lockfiles include it. No frontend tests are changed.

## Evidence

The PR description reports:

- The author ran `bazel test --build_tests_only //backend/...`; result: pass, including the new migration/helper and patch-path rejection unit tests.
- The author ran `bazel build //backend:backend`; result: pass.
- The author ran `./tools/checks run`; result: no findings.
- The author ran `npm --prefix frontend run build` (`tsc && vite build`); result: pass.

No screenshots are registered.
