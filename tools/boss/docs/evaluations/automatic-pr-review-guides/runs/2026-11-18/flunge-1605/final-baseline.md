1. **The problem is that the licence belonged to an account, even though it identifies a fencer.**

   Previously, British Fencing membership numbers were stored in each user’s `trackedFencers[]` entry. If two parents tracked the same child and only one entered the number, only that parent’s account—and their share links—could resolve British standings.

   This was intentional in the original design: independently entered numbers were treated as private assertions. PR #1605 changes that contract to **one shared licence per fencer, editable by any account tracking that fencer**. Corrections and clears affect every tracker.

   Sharing the value does **not** make British standings public. Viewing still requires either an authenticated tracker or a live share link whose owner tracks the bound fencer.

2. **The core implementation moves storage into a separate fencer-licence collection while retaining tracking as the permission check.**

   The new [`fencer_licence` crate](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/model.rs#L5) defines the shared record:

   ```rust
   pub struct FencerLicenceFields {
     pub fencer_id: TypedId<FencerId>,
     #[serde(default, skip_serializing_if = "Option::is_none")]
     pub membership_id: Option<String>,
     pub set_by: TypedId<UserId>,
     pub set_at: DateTime<Utc>,
   }
   ```

   Its [repository](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L14) uses the deterministic document ID `bfl-{fencerViewId}` in the new `fencer_british_licences` Cosmos collection. Writes upsert that document: last write wins. Clearing retains a document with no membership number and records who cleared it and when; this is latest-write provenance, not an audit history.

   The separate collection matters. `SyncFencerInfo` updates scraper-owned `FencerFields`; it cannot overwrite this licence through its existing merge-and-patch path. A licence can also exist without a corresponding stored fencer profile.

   The dependency direction remains straightforward: `blob` owns database wiring and request orchestration; `fencer_licence` owns the model, repository and pure migration planner. Repository functions accept a collection instead of depending on `blob`.

3. **The main review path is write → shared lookup → existing standings assembly, followed by compatibility and migration.**

   **Start with the write path.** The existing `PUT /api/v3/user/tracked-fencers/{fencerId}/british-fencing-membership` endpoint keeps its validation: the caller must track the fencer, input is trimmed, null/blank clears, and nonempty values over 32 characters are rejected.

   The decisive change is in [`set_tracked_fencer_british_fencing_licence`](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L249). After checking the current user document’s tracked list, it now calls:

   ```rust
   crate::fencer_licence::set_licence(
     &typed_fencer_id,
     licence,
     &ViewTypedId::<UserId>::database(user_id),
   )
   .await?;
   ```

   Previously, it changed `tracked.british_fencing_licence` and patched the user document. That user-document write and its cache invalidation are removed.

   **Next, inspect the read-side separation of permission and value.** The [old lookup](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/view/standings/fencer.rs#L755) returned the matching account-local field:

   ```rust
   tracked
     .iter()
     .find(|tracked| &tracked.fencer_id == fencer_id)
     .and_then(|tracked| tracked.british_fencing_licence.clone())
   ```

   The [new lookup](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L769) first checks tracking, then loads the shared record:

   ```rust
   if !may_resolve_british_licence(tracked, fencer_id) {
     return None;
   }
   match crate::fencer_licence::get_membership_id(fencer_id).await {
     Ok(licence) => licence,
     // The error branch logs a warning and returns None.
   }
   ```

   This makes `licence_for_fencer` asynchronous. Standings, composition and history handlers now await it. Share standings and composition do the same using the **link owner’s** tracked list and the **link’s bound fencer ID**. The ranking lookup and points calculation themselves are unchanged.

   **Worked example.** The following account setup is illustrative; the membership number, rank and points come from the existing [`british_standing_item_for_row_reports_scope_key_and_entrant_count` fixture](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer_british_tests.rs#L422).

   Alice and Bob both track `f.u.100344622`. Neither initially has a licence. Assume British rankings are enabled, database/cache reads succeed, the published cadet women’s sabre table and index contain licence `142013`, and no cached negative composition verdict suppresses its standing. Alice sends:

   ```http
   PUT /api/v3/user/tracked-fencers/f.u.100344622/british-fencing-membership
   Content-Type: application/json

   {"membershipId":" 142013 "}
   ```

   | Step                    | Before                                                                 | After                                                                                                                                  |
   | ----------------------- | ---------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
   | Normalize and authorize | Tracking check passes; input becomes `"142013"`.                       | Same.                                                                                                                                  |
   | Persist                 | Alice’s tracked-fencer entry receives the number. Bob’s remains unset. | `bfl-f.u.100344622` receives `membershipId: "142013"`, `setBy: "u.d.alice"` and the write timestamp. Neither user document is patched. |
   | Bob requests standings  | His account-local lookup returns `None`.                               | His tracking check passes, and the shared lookup returns `Some("142013")`.                                                             |
   | Assemble British items  | The optional British branch is skipped.                                | The existing licence-index lookup finds the table row and constructs its standing.                                                     |
   | Observable result       | Bob receives no British item.                                          | Bob receives the British `s.d.gb-CDTWS` item: rank **12**, points **37,210**. His next `/user/me` response also contains `"142013"`.   |

   The decisive existing assembly branch is:

   ```rust
   if let Some(licence) = british_licence {
     items.extend(british_standing_items(licence).await);
   }
   ```

   Alice’s write now changes what reaches that branch for Bob. No copying between accounts is needed.

   The useful boundary is a signed-out request without a share token: its handler still supplies `None`, so the same shared record produces **no British item** for that visitor. Likewise, a signed-in account that does not track this fencer fails the lookup gate.

   **Profile compatibility preserves the settings editor.** [`hydrate_user`](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L60) collects tracked IDs and loads their sidecar records through one filtered query using `cosmos::id_in`. It overwrites the response’s `trackedFencers[].britishFencingLicence` values without persisting them. Both GET `/user/me` and the generic PATCH response use hydration.

   A sidecar-read failure yields unset licence fields while preserving the rest of the profile; stale per-user values are not used as fallback. The existing settings editor already refreshes `/user/me` after saving, so it needs no new read endpoint.

   The [generic patch validator](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L497) also rejects operation paths containing `britishFencingLicence`, independently of the tracked-fencer lockdown flag. Its scope is worth reading precisely: this is a **path check**, not recursive inspection of replacement values.

   **Migration preserves uncertain historical data.** The explicit `migrate_british_fencing_licences` admin task invokes the [migration orchestration](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L149):

   - One distinct historical number: adopt it, attributing it to the lexicographically smallest asserting user ID.
   - An adoption candidate already has a shared record: skip the write, including when that record represents an explicit clear.
   - Multiple distinct numbers: make no shared write, log the fencer ID and distinct count, and retain the competing per-user values.
   - Strip other historical licence fields, patch affected users and invalidate their caches.

   Conflicting values remain stored for reconciliation, but hydration does not expose them as the current shared value. The task is deliberately not run during startup because it scans all users.

   **History capture becomes simpler.** [Connected-fencer enumeration](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L816) now joins tracked USAF IDs with shared licence records. Capture uses a `HashMap<u32, String>` instead of grouping competing account assertions. The ambiguity resolver, its counter and the admin UI’s “licence-ambiguous” display disappear. Historical disagreement is handled during migration rather than on every capture run.

4. **The tests mainly pin pure decisions, serialization and separation from scraper-owned data.**

   The new crate adds **10 unit tests**: six for migration planning, two for model serialization and two for deterministic document IDs. They cover agreement versus conflict, deterministic attribution, independent fencers, conflict counts without membership values in the conflict representation, and explicit-clear serialization.

   The blob layer adds **five migration-helper tests** for collecting historical assertions and stripping fields while preserving conflicts. It also adds **two scraper-isolation tests**: `FencerFields` must serialize without a licence key, and applying the real `merge_into` must produce no licence-related JSON patch operation.

   The [standings tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer_british_tests.rs#L540) replace three account-local lookup tests with tracking-gate tests. The central assertion is that **a tracker who entered nothing still passes the gate**. A new generic-patch test verifies rejection with lockdown disabled. Eight obsolete capture-ambiguity tests are removed, and capture-count serialization/summary tests are updated.

   Test infrastructure changes are small: the new [`//backend/fencer_licence:fencer_licence_test`](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/BUILD#L16) Bazel target, Cargo workspace/dependency wiring, lockfile updates and local fixture helpers. The new coverage uses pure functions and serialization; it does not introduce a Cosmos test harness.

   **Coverage limit:** despite the PR description’s reference to hydration tests, the added blob tests exercise `collect_assertions` and `strip_licence_except`, not `hydrate_user`. There is no added end-to-end two-account test, batch-read failure test or database-backed migration test.

   The PR reports passing backend tests/build, repository checks and the frontend build. I inspected the pinned before/after source; I did not rerun those commands.
