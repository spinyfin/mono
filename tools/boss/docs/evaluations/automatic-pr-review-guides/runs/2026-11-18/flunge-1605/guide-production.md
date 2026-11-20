# Shared British Fencing licences across tracking accounts

## Orientation

Two parents following the same child could see different British standings: only the account that entered the membership number could see them. This change makes one entry available to everyone tracking that fencer, so either parent can supply or correct it. A licence is the British Fencing membership number used to find the fencer in published rankings; tracking means adding that fencer to an account’s connected-fencer list.

The change lives in the backend services serving settings, rankings, and shared results. The latest saved licence becomes the shared value, including when someone clears it. Viewing British standings still requires a signed-in account that tracks the fencer, or a live results-share link whose owner tracks that fencer. A results-share link grants access to results for one particular fencer without requiring the recipient to sign in.

The existing British rankings feature brings published rankings and their points breakdowns onto the Results page alongside USA Fencing standings. This PR changes the account-specific membership-number connection underneath that feature, reusing its existing editor and ranking displays. The [design’s decision reversal](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/docs/design-docs/british-fencing-rankings-as-a-standings-source.md#L266-L303) explains the two-parent case and why shared storage remains separate from automatically refreshed fencer profiles.

## Change overview

This corrects the inconsistent experience across accounts by changing licence ownership from an account annotation to a fencer property. Any tracker can set, replace, or clear the value; the record stores who last wrote it and when. Sharing the value does not make British standings public to everyone who knows a fencer identifier.

Existing account values move through an explicit migration: agreement produces a shared licence, while disagreement preserves the old assertions for human reconciliation. Background standings-history capture then consumes one shared value per fencer instead of resolving competing account assertions.

## Core implementation

A new `fencer_licence` Rust package owns the record model, database operations, and migration planner. Its database collection is supplied by the caller, keeping it independent of `blob`, the backend application package. `blob` connects that package to writes, profile responses, standings reads, and the migration task. See the [package boundary](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/lib.rs#L1-L18).

The licence lives in a **sidecar**, meaning a separate database record associated with the fencer, in the Cosmos document database collection `fencer_british_licences`. Settings keep the existing response field through **hydration**: filling the response from shared storage immediately before returning it.

## Implementation walkthrough

### 1. Save one record per fencer through the existing editor

The existing `PUT /api/v3/user/tracked-fencers/{fencerId}/british-fencing-membership` route still checks that the authenticated account tracks the fencer. It trims input, accepts up to 32 characters, treats null or whitespace-only input as clearing, and returns 204 after success. These are [existing route semantics](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v3/user.rs#L51-L109), rather than new membership verification.

The decisive write change is in [the user helper](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L249-L275): after checking tracking again, it calls `set_licence` instead of patching the user document. The [application wrapper](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L30-L43) supplies the writer and current time.

The [stored model](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/model.rs#L9-L17) is small:

```rust
pub struct FencerLicenceFields {
  pub fencer_id: TypedId<FencerId>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub membership_id: Option<String>,
  pub set_by: TypedId<UserId>,
  pub set_at: DateTime<Utc>,
}
```

`setBy` and `setAt` identify the latest writer and write time. The [repository](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L15-L28) derives the key `bfl-{fencerId}`. Its [upsert](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L79-L94)—create or replace at that key—implements last-write-wins behavior. Clearing keeps a record with no membership number and the latest provenance.

This storage boundary matters because the existing [profile refresh](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencingtracker/mod.rs#L438-L486) merges imported profile data and patches `DB.fencers`. It does not write the licence collection, and saving a licence does not require a corresponding profile document.

### 2. Preserve the settings response while changing its source

[Both GET `/user/me` and the generic profile PATCH response](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/app.rs#L808-L834) now hydrate licences. The [repository batches tracked fencer identifiers into one filtered query](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L43-L77), avoiding a lookup per fencer. An empty tracked list needs no query.

[Hydration overwrites every response value](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L69-L91), including old account-local values:

```rust
for tracked in &mut user.fields.tracked_fencers {
  tracked.british_fencing_licence = by_fencer.get(&tracked.fencer_id).cloned().flatten();
}
```

A shared-storage read error becomes an empty map, so the profile still returns with those fields unset. The [existing settings input](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/frontend/src/SettingsPage.tsx#L455-L464) can continue reading `britishFencingLicence`, calling the dedicated write route, and refreshing the account response.

The [generic JSON patch validator](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L470-L512) additionally rejects operation paths containing `britishFencingLicence`, independent of the setting that blocks changes to tracked fencers. This is a check on each operation’s target path.

### 3. Resolve shared data only after checking viewing permission

The [new resolver](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L748-L787) separates the tracking check from fetching the number:

```rust
if !may_resolve_british_licence(tracked, fencer_id) {
  return None;
}
```

Only after that check does it load the shared record. Missing, cleared, and failed reads all produce no licence. The tracking check uses the [existing exact fencer-identifier comparison](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/share/mod.rs#L43-L50).

[Standings and composition handlers](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L1398-L1433) await this resolver using the signed-in account. Composition means the competition-by-competition breakdown of ranking points. The [history handler](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer_history.rs#L133-L151) does the same before finding the requested current standing.

[Shared standings](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/share/view.rs#L256-L290) and [shared composition](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/share/view.rs#L301-L343) instead supply the live link owner’s tracking list and the fencer bound to that link. A number entered by another tracker therefore works on the owner’s share page without changing which fencer the link grants access to.

### Worked example: one parent enters the number

Illustrative input: Alice and Bob both track `f.u.100344622`; neither initially has a licence. Alice submits `{"membershipId":" 142013 "}`. Assume British rankings are enabled and a published table contains that licence.

1. **Before:** the [old write helper](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/v2/user.rs#L246-L268) stored the trimmed number only in Alice’s user document. The [old standings resolver](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/view/standings/fencer.rs#L756-L764) read the requesting account’s field. Bob’s value remained absent, so his response omitted British standings.
2. **Now:** the same [route normalization](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v3/user.rs#L65-L108) yields `142013`, and the shared write stores it at `bfl-f.u.100344622`, attributed to Alice. On Bob’s next profile read, hydration supplies that number. On his next standings read, tracking passes the gate even though he entered nothing, and the resolver loads the same record.
3. **Visible result:** both accounts can receive the British ranking item for that licence. The [unchanged ranking assembly](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer.rs#L834-L911) appends British items only when a licence is supplied, the runtime switch enables British rankings, and matching published data is available.

If Bob subsequently clears the field, the shared record becomes cleared for both accounts. An ordinary signed-out request still passes no licence to assembly, even while the shared record contains a number; sharing storage does not bypass the viewing gate.

### 4. Migrate old assertions, then simplify history capture

The [pure migration planner](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/migrate.rs#L66-L111) groups account assertions by fencer and distinct licence. One distinct number produces an adoption decision, with the alphabetically first asserting user identifier selected as provenance. Multiple distinct numbers produce a conflict containing only the fencer identifier and distinct count.

The [migration orchestration](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L149-L220) scans users and existing shared records, skips adoption for identifiers already found in shared storage, and writes the other agreed values. Conflict decisions create no shared value; their warnings contain the fencer identifier and count. Conflicted fencers remain in a preservation set, so their old account values survive cleanup. Other old values are removed by patching the affected user documents and invalidating their cached copies.

The operation is exposed as the [explicit admin task `migrate_british_fencing_licences`](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/admintasks/src/migrate_british_fencing_licences.rs#L10-L31). [Startup deliberately does not run it](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/app.rs#L85-L90), avoiding repeated full user scans on every backend process start.

For history capture, [enumeration now combines connected USA Fencing identifiers with shared licence records](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L816-L834), excluding unconnected fencers and cleared values. [Capture builds a direct identifier-to-licence map](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/standings_history/capture.rs#L63-L115). The old disagreement resolver and `britishLicenceAmbiguous` diagnostic disappear; conflict handling now belongs to migration of historical assertions.

## Tests and test infrastructure

**Added tests:**

- [Migration planner tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/migrate.rs#L134-L230) cover empty input, a single assertion, matching assertions with deterministic provenance, disagreement without exposing licence values in conflict debug output, independent fencers, and three distinct values.
- [Application migration helper tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencer_licence.rs#L264-L328) check collecting only present assertions, retaining user/fencer associations, avoiding unnecessary cleanup writes, stripping ordinary values, and preserving a conflicted fencer while stripping another. Local user and tracked-fencer builders provide the fixtures.
- [Model tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/model.rs#L45-L74) cover omitted and populated membership fields through serialization; [repository tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/src/repository.rs#L105-L120) check stable, prefixed record keys.
- [Refresh regression tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/fencingtracker/mod.rs#L821-L888) use a sample imported profile and the existing fencer fixture to assert that profile JSON contains no licence field, refresh patches contain no licence-related paths, and the sidecar key differs from the profile document key. The [patch-path regression](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/v2/user.rs#L592-L605) rejects a direct licence-field replacement with tracked-fencer lockdown disabled.

**Modified tests:** the [former account-value lookup tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/view/standings/fencer_british_tests.rs#L540-L576) now assert the tracking gate, including acceptance when the tracker has entered nothing. [Capture diagnostic tests](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/blob/src/standings_history/state.rs#L352-L379) retain captured/disabled reporting and assert that `britishLicenceAmbiguous` is absent from serialized output.

**Removed tests:** the [old capture grouping and disagreement tests](https://github.com/brianduff/flunge/blob/9806138106ee6c5da6840b58b468bbc4f3e0b8af/backend/blob/src/standings_history/capture.rs#L642-L722) disappear with the removed multi-account resolution logic. They previously covered no assertions, agreement, deduplication, disagreement, and ambiguity counts.

**Build support:** the new package has a [Bazel library and `fencer_licence_test` target](https://github.com/brianduff/flunge/blob/256630b62986cc513c42e7e9075e57011707ce9a/backend/fencer_licence/BUILD#L1-L21), plus Cargo registration and the backend dependency. The tests use in-memory records, serialization, and pure helpers.

## Evidence

- The author ran `bazel test --build_tests_only //backend/...`; result: pass, including the new `collect_assertions`, `strip_licence_except`, and licence patch-path rejection tests.
- The author ran `bazel build //backend:backend`; result: pass.
- The author ran `./tools/checks run`; result: no findings.
- The author ran `npm --prefix frontend run build` (`tsc && vite build`); result: pass.

No screenshots are registered.
