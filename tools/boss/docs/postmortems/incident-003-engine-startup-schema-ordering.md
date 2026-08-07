# Incident 003 — A bootstrap index statement ran before the migration that supplied its column, and every engine start died

- **Date:** 2026-08-06, ~12:36–12:47 local. Engine crash-looped 12:36:25 → 12:42:40 (11 recorded failures); recovered 12:47:06.
- **Severity:** High — total outage of Boss on the operator's machine. The macOS app and every `boss` / `bossctl` command were unusable for ~11 minutes. No data was lost or corrupted.
- **Trigger:** `boss-v1.0.489` (commit `d15d0949a449`, [#2642](https://github.com/spinyfin/mono/pull/2642), "Add durable tmux run session records"), released 2026-08-03 02:11 local — ~2 minutes after merge.
- **Class:** Second occurrence in 26 hours of _schema DDL that is correct against a blank database and fatal against a database with history_. The first was [#2634](https://github.com/spinyfin/mono/pull/2634) → [#2635](https://github.com/spinyfin/mono/pull/2635) on 2026-08-01. See §5.
- **Status:** Recovered by an operator-authorised manual `ALTER TABLE` against a backed-up live database. The code fix for the ordering defect is tracked separately; this document does not duplicate it.
- **Related:** [`incident-001-pr-fan-out.md`](incident-001-pr-fan-out.md) (whose action item #5 built the feature-flag system discussed in §8), [`incident-002-merge-conflict-deletion-blessed-by-review.md`](incident-002-merge-conflict-deletion-blessed-by-review.md).

## 1. Verdict

The engine did not lack a migration. A correct, properly-guarded additive migration for the tmux columns was written, in the same commit, and is still in the tree. It never ran, because the same commit also placed a `CREATE UNIQUE INDEX` on one of those columns into the bootstrap statement batch that executes ~400 lines earlier — killing the process before its own migration was reached.

The defect is **statement ordering within a single change**, not omission. "The agent forgot the migration" is factually wrong and should not survive into anyone's mental model of this incident.

What let it ship is more interesting than the defect: the batch conflates historical baseline DDL with current schema definition, the invariant that governs it is unwritten, and every test that guards it runs against a blank database — including a test whose name reads like total schema coverage.

## 2. What the operator saw

The app launched normally and sat at "Disconnected" with an empty board reading "No work items yet" — visually indistinguishable from a wiped database. The only failure indicator in the UI is a caption-sized red dot and the word "Disconnected" in the sidebar footer (`tools/boss/app-macos/Sources/ContentView.swift:723-729`).

Every CLI call reported a symptom with no cause:

```
error: boss engine did not become ready at /tmp/boss-engine.sock within 5 seconds
```

`/tmp/boss-engine.sock` existed but was stale (dated Aug 2 23:40, the last clean run). Removing it did not help and the engine did not recreate it — which correctly established that the engine was dying _before_ bind.

The real error was obtainable only by running the bundled binary by hand:

```
INFO boss-engine logging initialized log_path=/tmp/boss-engine.log
INFO starting boss-engine runtime cwd=/private/tmp db_path=~/Library/Application Support/Boss/state.db ...
INFO engine-control token: ready token_path=.../engine-control.token
Error: no such column: tmux_spawn_token in
            CREATE UNIQUE INDEX IF NOT EXISTS work_runs_tmux_spawn_token_idx
                ON work_runs(tmux_spawn_token)
                WHERE tmux_spawn_token IS NOT NULL;
            [... long multi-statement schema batch continues ...]
             at offset 107

Caused by:
    Error code 1: SQL error or missing database
```

## 3. Root cause

### 3.1 The mechanism

`WorkDb::init()` (`tools/boss/engine/core/src/work/schema_init.rs:13-19`) routes on whether the database has any table at all:

```rust
pub(crate) fn init(&self) -> Result<()> {
    let conn = self.connect()?;
    if Self::has_any_existing_table(&conn)? {
        return Self::run_full_migration_chain(&conn);
    }
    Self::apply_final_schema_template(&conn)
}
```

A database with history therefore takes `run_full_migration_chain` (`schema_init.rs:102`). That function opens with a **single `execute_batch`** spanning `schema_init.rs:103-296` — roughly 190 lines of `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS` — and only _then_, from line 297 onward, calls ~80 incremental `migrate_*` functions.

Inside that batch, 25 lines apart:

```sql
CREATE TABLE IF NOT EXISTS work_runs (
    ...
    tmux_server_label TEXT,      -- schema_init.rs:227
    tmux_session_name TEXT,
    tmux_spawn_token TEXT,       -- schema_init.rs:229
    tmux_spawn_state TEXT,
    tmux_pane_pid INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS work_runs_tmux_spawn_token_idx   -- schema_init.rs:236-238
    ON work_runs(tmux_spawn_token)
    WHERE tmux_spawn_token IS NOT NULL;
```

The two statements have completely different semantics against an existing database, and the visual grouping hides it:

- `CREATE TABLE IF NOT EXISTS` **silently no-ops**. The five columns are never added.
- `CREATE UNIQUE INDEX IF NOT EXISTS` is idempotent against a pre-existing _index_, but not against a missing _column_. It resolves `tmux_spawn_token`, finds nothing, and aborts the batch.

`WorkDb::open` returns `Err`, the error propagates through `app::run` to `main`, and the process exits before either socket is bound.

### 3.2 The migration existed and was correct

`migrate_work_runs_tmux_columns` (`tools/boss/engine/core/src/work/migrations_a.rs:487-506`) does exactly the right thing — guarded, additive, nullable, with the index created _after_ the columns it depends on:

```rust
pub(crate) fn migrate_work_runs_tmux_columns(conn: &Connection) -> Result<()> {
    for (column, sql_type) in [
        ("tmux_server_label", "TEXT"),
        ("tmux_session_name", "TEXT"),
        ("tmux_spawn_token", "TEXT"),
        ("tmux_spawn_state", "TEXT"),
        ("tmux_pane_pid", "INTEGER"),
    ] {
        if !table_has_column(conn, "work_runs", column)? {
            conn.execute(&format!("ALTER TABLE work_runs ADD COLUMN {column} {sql_type}"), [])?;
        }
    }
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS work_runs_tmux_spawn_token_idx
         ON work_runs(tmux_spawn_token)
         WHERE tmux_spawn_token IS NOT NULL",
        [],
    )?;
    Ok(())
}
```

It is invoked at `schema_init.rs:712` — **415 lines after the statement that already killed the process.**

The decisive detail: the `CREATE UNIQUE INDEX` statement is **duplicated**. The same index appears once correctly (inside the migration, reachable only after the columns exist) and once fatally (inside the bootstrap batch, reachable before).

Deleting the batch copy would have stopped this outage, but it is not what shipped, and it would not have closed the class — the duplication is systemic (§3.5). The shipped fix instead makes the batch structurally incapable of failing on an existing database:

- **Split the batch.** Baseline DDL is now tables only. Baseline index DDL runs _after_ the full migration chain, over the already-migrated column set, so no index statement can ever resolve a column the chain has not yet supplied.
- **Folded in the exceptions.** Two indexes had previously been hand-hoisted out of the batch for this same reason. They now follow the one rule, so there is one rule instead of a rule plus two exceptions.
- **Removed the duplicated column declarations.** All 40 columns that the baseline and a migration both supplied (§3.5) are gone from the baseline, so each column has a single owner.
- **Added structural guard tests.** One asserts the baseline declares no indexes at all; one detects baseline/migration column duplication by construction. Reintroducing either shape fails the build rather than shipping.

### 3.3 Reproduced

The mechanism reproduces in two `sqlite3` commands, with no engine involved. Create a `work_runs` at the pre-#2642 shape, then replay the shipped batch's two relevant statements:

```
$ sqlite3 repro.db "CREATE TABLE work_runs (id TEXT PRIMARY KEY, ..., shell_pid INTEGER);"
$ sqlite3 repro.db "CREATE TABLE IF NOT EXISTS work_runs (..., tmux_spawn_token TEXT, ...);
                    CREATE UNIQUE INDEX IF NOT EXISTS work_runs_tmux_spawn_token_idx
                      ON work_runs(tmux_spawn_token) WHERE tmux_spawn_token IS NOT NULL;"
Error: in prepare, no such column: tmux_spawn_token
exit=1
$ sqlite3 repro.db "PRAGMA table_info(work_runs);"
0|id|TEXT|0||1
1|execution_id|TEXT|1||0
2|agent_id|TEXT|1||0
3|status|TEXT|1||0
4|shell_pid|INTEGER|0||0
```

Note the second command's output: the `CREATE TABLE IF NOT EXISTS` added **nothing**, silently, and the very next statement died on the column it was supposed to have created.

### 3.4 Blast radius is inverted

Fresh databases — every developer machine, every CI run, every new install — take `apply_final_schema_template` (or run the chain against an empty file), create `work_runs` _with_ the five columns, and start cleanly. Only a database with history breaks, and it breaks deterministically, every single start.

The installs with the most real work recorded in them are the ones guaranteed to fail. No amount of local testing on a scratch database can see it.

### 3.5 The generator: 40 columns are declared twice

One duplicated index caused this outage. It is not the interesting number. The fix work measured the class exhaustively: for each of the **577 columns** in the canonical schema, build a fresh database via the chain, drop that one column, re-run the chain, and record both whether the column comes back and whether the chain errors. Two distinct populations fall out.

- **Index-ordering class — exactly one column.** `work_runs.tmux_spawn_token` is the _only_ column in the entire schema whose absence makes the bootstrap batch itself abort. This outage sat on a population of one. That is luck, not margin: it means the failure mode was one statement away from existing anywhere else in the file, and nothing was preventing it.
- **Column-duplication class — 40 columns.** Forty columns are declared in the bootstrap batch _and_ supplied by a `migrate_*` function: the five `tmux_*` columns plus 35 across `products`, `projects`, `tasks`, `work_executions`, `work_runs`, and `work_attention_items`.

The second number is the generator of this bug class, and the mechanism is worth naming precisely:

> The baseline declaration only ever helps a **fresh** database. The migration is what upgrades **everyone else**. When both exist, the fresh path is served entirely by the baseline and never depends on the migration at all.

So for any of those 40 columns, a migration that is broken, mis-guarded, or simply unreachable is **invisible on every developer machine and every CI run**, and visible only on installs with history. The duplication does not merely fail to help; it actively suppresses the signal, because it guarantees the only databases anyone tests against take the path that cannot notice. That is what makes this class recurrent rather than incidental — 40 independent opportunities for the same shape of defect, each one green in CI by construction.

The single duplicated index was one of the 40 that happened to also be fatal. Removing it addresses the outage; removing the duplication addresses the class.

## 4. Recovery, and what it proves

Operator-authorised remediation on the live 128 MB `state.db`, after taking a full backup (`state.db.bak-20260806-124633`):

```sql
ALTER TABLE work_runs ADD COLUMN tmux_spawn_token TEXT;
```

The engine started cleanly on the next attempt.

**All five tmux columns had drifted, not one.** Pre-remediation, `work_runs` had 27 columns ending at `progress_ingress_checkpoint`; none of the five were present. The manual `ALTER` added only `tmux_spawn_token` — enough to unblock the batch. The engine then reached `migrate_work_runs_tmux_columns`, which skipped the now-present `tmux_spawn_token` via its `table_has_column` guard and appended the remaining four in declared order as columns 28–31, plus the index. The resulting column ordering (27 = `tmux_spawn_token`, 28–31 = the rest) is itself the evidence for that sequence.

This is the most useful fact in the whole incident: **the engine's own migration completed the repair.** The hand-written `ALTER` was not a substitute for the migration — it was merely the key that unblocked the batch so the correct, already-written migration could run. That is a direct argument for fixing the ordering rather than ever hand-patching a user's database, and against any remedy that treats manual SQL as the recovery path.

It also means the operator's earlier read that only one column had drifted was understandable but wrong, and the correction matters: had `tmux_spawn_token` not happened to be the column the fatal statement named, a single-column `ALTER` would not have unblocked anything.

## 5. The earlier incident — evidence, and how it relates

The operator reported this was the second serious engine-start failure in a week. It was, and the earlier one is unambiguously identifiable.

**2026-08-01, [#2634](https://github.com/spinyfin/mono/pull/2634) → [#2635](https://github.com/spinyfin/mono/pull/2635) (`c285efe1f4e9`, tagged `boss-v1.0.486`, 23:27 local).** The fix commit's title states the impact directly: _"Fix the status CHECK migration that wedges every engine start."_ Its body describes the mechanism:

> The projects/tasks status CHECK migration rebuilds both tables via CREATE \_v2 / INSERT SELECT / DROP / RENAME, issued as one bare execute_batch. […] `projects` is an FK parent with live children […] so DROP TABLE projects runs an implicit DELETE FROM and aborts with FOREIGN KEY constraint failed. There is also no enclosing transaction, so the already-committed CREATE TABLE projects_v2 and its INSERT survive the abort. The idempotence guard then still sees an unconstrained `projects`, so every later start retries and now fails one statement earlier on "table projects_v2 already exists" — a hard boot loop.

### Same class, different defect

They are **independent defects in one recurring class**, 26 hours apart.

Different proximate cause:

|                 | Incident (2026-08-01)                                    | This incident (2026-08-06)                         |
| --------------- | -------------------------------------------------------- | -------------------------------------------------- |
| Migration       | Ran, and was wrong (FK enforcement + no transaction)     | Was right, and never ran                           |
| Fatal statement | Inside a `migrate_*` function                            | Inside the bootstrap batch, before any `migrate_*` |
| Depends on      | `projects` having child **rows**                         | `work_runs` merely **existing**                    |
| After-effect    | Left a half-migrated DB that failed _worse_ each restart | Left the DB untouched                              |

Shared class — and this is the part that matters:

1. **Correct against a blank database, fatal against a database with history.** Both.
2. **Inverted blast radius.** Dev machines and CI immune; the operator's real install guaranteed to break. Both.
3. **Shipped through a green CI gate**, because nothing in that gate opens a database with history. Both.

### The aggravating finding

[#2635](https://github.com/spinyfin/mono/pull/2635) did not just fix its defect. It added **455 lines of old-database migration tests** to `tools/boss/engine/core/src/work/tests/schema_migration_tests.rs` — including `seed_legacy_db_with_fk_children`, `status_check_migration_rebuilds_tables_that_have_foreign_key_children`, and `status_check_migration_recovers_a_database_left_half_migrated`. It also established the correct SQLite 12-step table-rebuild recipe in `migrations_c.rs`.

The next schema-bearing change landed ~26 hours later and used none of it.

That is the real lesson of the pair: **the first fix was scoped to the defect, not to the class.** Excellent, specific remediation that left the door it came through wide open.

### Adjacent, not counted

Two other engine-start-affecting changes landed in the same window — `e03ebee9353b` ("Fix bundled engine autostart resolution", [#2638](https://github.com/spinyfin/mono/pull/2638), `boss-v1.0.495`) and `b327d91ab322` ("Supervise engine restarts from the macOS app", [#2641](https://github.com/spinyfin/mono/pull/2641), `boss-v1.0.494`). Neither is a schema failure and neither is a third instance of this class; they are noted because they show how much churn the engine's startup path absorbed that week, and because #2641 materially changed how this incident presented (§9.2).

## 6. Why the system permitted it

The operator asked what would have told the author the ordering was unsafe. Answering honestly requires separating what was there from what was not.

### 6.1 What the author got right

Everything except one line. Reading [#2642](https://github.com/spinyfin/mono/pull/2642)'s diff, the author clearly understood the two-path model: they added the columns to the canonical `CREATE TABLE` (correct for the fresh path), wrote a guarded `ALTER TABLE` migration (correct for the upgrade path), wired it into the chain, and documented the intent in a comment at `schema_init.rs:707-709`. The columns and the index were each added to _both_ places. Nothing landed separately; it was one commit.

### 6.2 Why the diff looked right

The change is **symmetric**, and symmetry is exactly what a reviewer scans for in a schema diff: columns in both places, index in both places. The asymmetry that kills — that one copy is reachable before the columns exist and the other only after — is invisible in the diff, because the two locations are 400 lines apart in a 996-line file and the batch gives no signal about which era it belongs to.

### 6.3 The invariant is unwritten, and the written one is satisfied

`run_full_migration_chain`'s own doc comment (`schema_init.rs:93-101`) states the rule the author would have read:

> every step here must stay idempotent against its own prior output, since `init()` can run it again on an already-current database.

`CREATE UNIQUE INDEX IF NOT EXISTS` **is** idempotent against its own prior output. The stated invariant is fully satisfied by the statement that caused the outage.

The invariant that actually governs the batch is different and appears nowhere in the repo:

> Every statement in the bootstrap batch must be valid against the **oldest** supported database, because it runs before any migration.

### 6.4 The batch conflates two things

`run_full_migration_chain` is doing two jobs with one construct. Its opening `execute_batch` is simultaneously (a) the historical baseline schema — what a database from the beginning of time looked like — and (b) the place people naturally edit when adding a table or column, because it is where the `CREATE TABLE` lives. Job (a) demands it be frozen. Job (b) invites edits. Nothing in the code says which job wins.

### 6.5 Nothing mechanical would have caught it

Searched and confirmed absent:

- No rule about schema changes or migrations in the root `AGENTS.md` or `CLAUDE.md`, or in any `tools/boss/` equivalent.
- No check in any `CHECKS.yaml` (root, `tools/boss/`, `tools/checkleft/`) touching schema, migrations, or SQLite.
- No PR checklist, no lint, no compile-time guard.

The only signals available were implicit and inferential: the shape of ~80 sibling `migrate_*` functions, and a 1325-line `schema_migration_tests.rs` that the author would have had to go looking for.

### 6.6 The coverage that looked total

This is the sharpest contributor. `schema_init.rs` contains two tests that read as comprehensive schema coverage:

- `full_migration_chain_produces_current_schema` (`schema_init.rs:851`), documented as \_"the coverage of record for the real incremental migration chain […] the only place that actually exercises every `migrate\__`step in order"\* — and its very next line is`Connection::open_in_memory()`. It runs the chain against a **blank** database. The doc comment even says so: _"against a blank database."_
- `fresh_schema_template_matches_full_migration_chain` (`schema_init.rs:974`) compares chain output to template output. **Both blank.** It proves the two fresh paths agree; it says nothing about upgrades.

[#2642](https://github.com/spinyfin/mono/pull/2642) dutifully extended the first test — adding assertions that all five tmux columns and the index exist after the chain runs (`schema_init.rs:929-958`). Those assertions **pass**, because on a blank database the batch's `CREATE TABLE` really does create the columns. The author added coverage to the test named "coverage of record" and got a green result that was structurally incapable of failing.

The three tests added to `tests/t01.rs` by the same commit (`tmux_run_accessors_record_and_list_only_adoptable_local_runs`, `list_adoptable_tmux_runs_excludes_finished_run_of_live_execution`, `tmux_spawn_tokens_are_unique_across_runs`) each open a fresh `WorkDb::open(temp_db_path(...))`. Also blank.

**Every test written for this change passed, and none of them could have failed.** That is the blind spot in one sentence.

### 6.7 So: what permitted it

Not forgetfulness. A construct that mixes frozen history with live schema, governed by an invariant nobody wrote down, guarded by a test suite whose most authoritative-sounding member runs against the one input that cannot expose the bug — with no lint, no convention, and no review affordance to catch the difference.

## 7. The test that would have failed

The operator asked for a test that fails today, pre-fix, the same way the incident failed. There are three tiers; the repo already has everything needed for the first.

### 7.1 Tier 1 — the direct regression test (write this now)

`schema_migration_tests.rs` already contains the exact pattern, ten times over. `migration_re_adds_effort_and_model_columns_on_upgrade` (lines 49-117) is the template: open a DB, drop the columns to simulate the older schema, close, re-open so `init()` replays the chain, assert.

The tmux equivalent, in the same file:

```rust
/// Drop the tmux run-session columns and their index (simulating a
/// pre-#2642 database) and re-open: the chain must bring the database
/// up without dying, and `migrate_work_runs_tmux_columns` must re-add
/// all five columns plus the partial unique index.
#[test]
fn migration_re_adds_tmux_columns_on_upgrade() {
    // disk_db_path required: drops columns and re-opens the DB to trigger migration.
    let (_dir, path) = disk_db_path("tmux-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "Boss", Some("git@github.com:test/repo.git"));
    let chore = create_test_chore(&db, product.id.clone(), "Legacy chore");
    // …seed an execution + work_run so the table has history, not just a schema…

    {
        let conn = db.connect().unwrap();
        // Order matters: SQLite refuses DROP COLUMN while an index
        // references the column. Dropping the index first reproduces
        // the exact pre-#2642 shape.
        conn.execute("DROP INDEX work_runs_tmux_spawn_token_idx", []).unwrap();
        for column in [
            "tmux_server_label", "tmux_session_name",
            "tmux_spawn_token", "tmux_spawn_state", "tmux_pane_pid",
        ] {
            conn.execute(&format!("ALTER TABLE work_runs DROP COLUMN {column}"), []).unwrap();
        }
    }
    drop(db);

    // Re-open replays the real chain. TODAY THIS LINE PANICS ON
    // Err("no such column: tmux_spawn_token") — the incident, verbatim.
    let db = WorkDb::open(path.clone()).unwrap();
    {
        let conn = db.connect().unwrap();
        for column in [
            "tmux_server_label", "tmux_session_name",
            "tmux_spawn_token", "tmux_spawn_state", "tmux_pane_pid",
        ] {
            assert!(table_has_column(&conn, "work_runs", column).unwrap(), "{column} not re-added");
        }
        // …assert the partial unique index is back, and the seeded run row survived…
    }
}
```

The drop ordering was verified empirically. With the index present, `ALTER TABLE work_runs DROP COLUMN tmux_spawn_token` fails with `error in index work_runs_tmux_spawn_token_idx after drop column: no such column: tmux_spawn_token`; dropping the index first succeeds. That is not incidental — the index-plus-column pair _is_ the pre-#2642 state, so reproducing it correctly is also documenting it.

Cost: ~40 lines, no new infrastructure, milliseconds to run. Value: catches this exact defect. Limitation: it is per-change and reactive — it only covers columns someone remembered to write a test for. It does not close the class.

### 7.2 Tier 2 — the generic upgrade guard (closes the class)

A single test that fails for _any_ future statement placed in the batch that depends on a migrated column:

```
for each fixture F in tests/fixtures/schema/*.sql:
    build a database from F
    WorkDb::open(it)                       // must be Ok — this is the incident's assertion
    assert sqlite_master matches the current template
```

The comparison helper already exists: the `capture()` closure in `fresh_schema_template_matches_full_migration_chain` (`schema_init.rs:983-995`) dumps `type:name:sql` from `sqlite_master`. Point it at an upgraded fixture instead of a second blank database and it becomes an upgrade-parity assertion rather than a fresh-path tautology.

**How to produce and maintain the fixtures.** Three options, assessed:

| Approach                                                                                             | Fidelity                                                                                                                                                                                                                                                                                  | Upkeep                                                                                   | Verdict                                                        |
| ---------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| **(a) Checked-in binary `.db` per release**                                                          | Highest — carries real rows, so it catches data-dependent failures like the 2026-08-01 FK-children abort                                                                                                                                                                                  | Binary blobs in git, ~50–200 KB each, one per release, someone must remember to cut them | Too heavy per-release; keep a _small number_ of "fat" fixtures |
| **(b) Replayed migration history** — run the chain truncated at step N                               | Lowest where it matters. Requires the chain to be re-entrant at arbitrary prefixes, and it can only reproduce schemas the chain itself produces — it _cannot_ reproduce what a shipped build actually left on disk (e.g. the half-migrated `projects_v2` state from the earlier incident) | Free                                                                                     | Rejected as the primary mechanism                              |
| **(c) Text DDL snapshot per release** — `sqlite3 state.db .schema > fixtures/schema/boss-v1.0.N.sql` | Catches all pure-DDL drift, including this incident. Carries no rows, so it would **not** have caught the earlier incident                                                                                                                                                                | Cheap; one line in the release script; text diffs review cleanly                         | Primary mechanism                                              |

**Recommendation: (c) as the always-on guard, plus (a) for a small, deliberately maintained set of "fat" fixtures** carrying representative rows including FK children — one or two is enough, regenerated only when row shapes change, with the recipe documented next to them. That combination covers both incidents in this class. (c) alone covers this one.

The one honest caveat: (c) snapshots are only as old as the day the practice starts. They cannot retroactively cover databases older than the first snapshot, so the fat fixtures should be cut from the oldest schema still plausibly in the field.

### 7.3 Tier 3 — the structural fix that makes the class unrepresentable

Tests catch defects; structure prevents them. The batch should be **frozen as the historical baseline it already is**, with every future schema change required to be a `migrate_*` function appended to the chain. Once frozen, a `checkleft` check can enforce it mechanically: any diff touching lines inside `run_full_migration_chain`'s literal batch fails, with a message pointing the author at `migrations_*.rs` instead.

That check would have failed [#2642](https://github.com/spinyfin/mono/pull/2642) in CI, at review time, naming the correct remedy — and the correct remedy was already written and sitting in the same commit. It is a changed-lines check on a single file, which is a shape this repo's linter already supports well.

### 7.4 CI and the release gate

**Every PR: yes.** Tiers 1 and 2 are ordinary unit tests under `bazel test //tools/boss/engine/...`, already in the `bazel-build-test` step, and cost milliseconds. There is no argument for making them conditional.

**The release path: yes, and it needs one thing CI cannot give.** `.buildkite/pipeline.yml:39-53` already gates `boss-release` on `bazel-build-test`, `mac-app-build`, and `checks`, so tiers 1–2 land in the release gate automatically. But every one of those steps exercises a _bazel-built test binary_, never the artifact that ships. The gap worth closing at release time is a **smoke start of the bundled engine binary from the built `Boss.app` against the newest fat fixture**, asserting it binds its socket and exits cleanly. That is the only check that tests what the user actually receives, and it belongs in `mac-app-build` where the bundle already exists.

## 8. How unfinished schema reached a shipped bundle

### 8.1 The feature was known-inert, and that was the trap

The tmux work is in development. [#2642](https://github.com/spinyfin/mono/pull/2642)'s own comment at `schema_init.rs:707-709` says so explicitly:

> The spawn path does not use these columns until the tmux-hosting rollout lands.

The author knew the feature was unreachable and documented it. It still bricked every engine with history. **Dead code is not inert when it is schema.** Schema executes at startup, unconditionally, before any feature gate can be consulted — which makes "it's not wired up yet" an argument for _more_ care with the DDL, not less.

### 8.2 The release path

`.buildkite/steps/boss-release.sh` builds `Boss.app`, auto-increments to the next `boss-v1.0.N`, and publishes a GitHub Release with the zip. `UpdateChecker` polls the unauthenticated GitHub Releases API, filters `boss-v*`, picks the maximum version, and `UpdateInstaller` swaps the bundle in place.

There is one channel. No beta, no canary, no staged rollout, no soak period, no opt-in for in-development work. `boss-v1.0.489` was tagged at 02:11 — the same minute #2642 merged — and `boss-v1.0.490` followed at 02:13.

**What the path gates:** CI green — build, mac app build, checks — plus an idempotency guard and a Boss-affecting-changes filter.

**What it does not gate:** whether the change is finished, whether it is reachable, whether it is safe against an existing install, or whether anyone has run it against a database with history.

The gate is real and it is blind to this entire class, because none of the three steps it depends on ever opens a database that has been used.

### 8.3 Feature flags — the infrastructure exists, and would not have saved this

Boss has a mature, first-class flag system in `tools/boss/engine/feature-flags/`: a `const REGISTRY` of `FeatureFlagSpec` entries (`lib.rs:84`), a `FeatureFlagsStore` mirrored to `~/Library/Application Support/Boss/feature-flags.toml`, `ListFeatureFlags` / `SetFeatureFlag` RPCs (`app.rs:2236`, `app.rs:2318`), a `FeatureFlagsViewer` debug pane in the app, and a `CapabilityRegistry` that warns when an operator enables a flag in a build lacking the implementation. Adding a flag is two steps and documented at `lib.rs:12-20`.

It was built as **action item #5 of incident 001** (`lib.rs:4-10`), for precisely this purpose:

> Any engine behaviour that is optional-for-correctness and carries non-trivial blast radius when wrong should be gated by a flag the human can flip from a debug pane without rebuilding the engine.

Flags already default OFF for in-development paths — `editorial_controls` and `attentions_questions_backstop` both do exactly what the operator describes. The tmux work used none of it.

**But a flag would not have prevented this outage, and the postmortem must be honest about that.** Flags gate _behaviour_. The store loads at `app.rs:1014`, after `WorkDb::open`. Schema bootstrap runs before any flag is readable — it must, since the flag store's own consumers need the database. Gating the tmux _spawn path_ behind a flag (which is the right thing to do, and the operator's instinct is correct) would have left the schema statements ungated and the engine would have died identically.

So the rule for schema-bearing in-development features has two halves, and only one of them is a flag:

1. **Behaviour goes behind a registry flag, default OFF, until the rollout lands.** One `FeatureFlagSpec` entry plus one `is_enabled` call. This is what the tmux work should have done, and it remains correct advice for the rest of that feature.
2. **Schema cannot be flagged, so it must be unconditionally safe.** Additive nullable columns applied through a guarded `migrate_*` function are safe to land ahead of the feature and are the right way to ship schema early. Anything that rebuilds a table, adds a constraint, or creates an index is _not_ additive in the relevant sense and must land with the rollout, not ahead of it.

The corollary is worth stating plainly, because it is easy to draw the wrong lesson: the mistake was not "shipped schema before the feature." Shipping inert additive schema early is a good pattern. The mistake was shipping it **via the one path that cannot tolerate it.**

### 8.4 Backward compatibility — the mechanism was adequate; the discipline around it was not

A migration mechanism exists and is mature: ~80 `migrate_*` functions across `migrations_a.rs`, `migrations_b.rs`, `migrations_c.rs`, `migrations_boothby.rs`; `table_has_column` / `table_exists` guards; an `init()` that routes fresh vs. existing; and 1325 lines of upgrade tests in `schema_migration_tests.rs`.

`metadata.schema_version` exists (currently `'31'`) but is deliberately **not** a dispatch key. `schema_init.rs:528-531`:

> `schema_version` is a coarse bookkeeping marker, not a per-migration dispatch key: additive `CREATE TABLE IF NOT EXISTS` migrations […] ride the current marker rather than bumping it.

Dispatch is per-statement idempotence, not version-gated laddering. That is a legitimate and common design for SQLite, and it is not what failed here. **Nothing about the mechanism needs replacing.** What it lacks is a written, enforced discipline about where new DDL may go.

The standard going forward:

1. The bootstrap batch is **frozen**. It is history, not a schema definition. No statement is ever added to it or edited within it.
2. Every schema change is a `migrate_*` function appended to the end of the chain, guarded by `table_has_column` / `table_exists` / `IF NOT EXISTS`.
3. New columns are `ALTER TABLE … ADD COLUMN`, nullable, no `NOT NULL` without a default.
4. Indexes on new columns live in the **same** `migrate_*` function, after the `ADD COLUMN` loop — exactly as `migrations_a.rs:499` already does.
5. Constraints requiring a table rebuild follow the SQLite 12-step procedure established by [#2635](https://github.com/spinyfin/mono/pull/2635) in `migrations_c.rs` — foreign keys off outside the transaction, whole rebuild inside one, `PRAGMA foreign_key_check` before commit.
6. Every schema change ships with an upgrade test in `schema_migration_tests.rs`.

## 9. Observability: the diagnosis was already written down, 11 times

This is the finding with the best effort-to-value ratio in the document.

### 9.1 The engine already records exactly why it died

`main.rs:205-213`:

```rust
let result = app::run(cli).await;
let reason = match &result {
    Ok(()) => "normal".to_owned(),
    Err(err) => format!("error:{}", short_error(err)),
};
audit::record_shutdown(reason);
result
```

The audit path is set at `main.rs:197-198`, **before** `app::run` — so it is live when the schema failure happens. `short_error` (`main.rs:316-327`) takes the error's first line, capped at 200 chars. For this failure that first line is `no such column: tmux_spawn_token in` — short, exact, and naming the column.

So `~/Library/Application Support/Boss/engine-audit.log` on the operator's machine contains, once per crash-loop iteration:

```json
{"event":"shutdown","reason":"error:no such column: tmux_spawn_token in","uptime_sec":0,"pid":…}
```

Eleven of them, written while the operator was running the bundled binary by hand to discover the same string. `engine-audit.log` is even documented as a forensic surface in the repo's own `CLAUDE.md` ([`forensic-surfaces.md`](../forensic-surfaces.md)). **The complete diagnosis was durably recorded, in a known location, by design — and nothing reads it.**

One limitation to note for other failures: `short_error` keeps only the first line, so a multi-line error is truncated to its head. It happened to be ideal here.

### 9.2 Why nothing else surfaced it

Three independent discards, any one of which would have exposed the cause:

1. **The fatal error never reaches the log file.** The engine returns it from `main() -> Result<()>` (`main.rs:121`), and Rust's `Termination` impl prints that to **stderr only**. The tracing layers write INFO to both stderr and `/tmp/boss-engine.log` (`main.rs:152-155`). So the log file contains every line _except_ the one that explains the outage — which is exactly why "three INFO lines then silence" is the signature of every engine startup failure, not just this one.
2. **The app discards the engine's stderr.** `EngineProcessController.swift:499` launches it as `nohup \(command) >/dev/null 2>&1 &`. Redirected to `/dev/null` before the process starts.
3. **The CLI discards it too.** `client/src/lib.rs:361` spawns with `.stderr(Stdio::null())`. Same discard, different code path, so neither entry point can report a cause.

The CLI's user-facing message (`client/src/lib.rs:228-232`) is therefore a bare timeout — a symptom with the cause deliberately thrown away moments earlier.

**Restart supervision, added between the two incidents, made it quieter.** [#2641](https://github.com/spinyfin/mono/pull/2641) (`boss-v1.0.494`, 2026-08-03) retries on a `[1, 2, 4, 8, 16, 30]`-second backoff with `maximumAttempts: 6`, then emits `"[engine supervision] gave up after N restart attempts; use Restart engine to try again"` (`EngineProcessController.swift:469`) — to the app's diagnostic sink, not the UI. A deterministic, unrecoverable startup failure now presents as patient silence. (The 11 recorded failures are consistent with the supervisor's 6-attempt budget plus the additional engines spawned by each `boss` / `bossctl` triage command, since every CLI invocation independently calls `ensure_engine_running`.)

### 9.3 The bug suppressed its own paper trail

`boss chore create` routes through `ensure_engine_running`, so the outage blocked filing the chore _about_ the outage. Any Boss defect that prevents engine start is self-concealing: it destroys its own incident record at the moment the record would be created.

This is a real property of the tooling, and it is verifiable from the code path alone: for the eleven minutes of this outage, the command that files an incident record was one of the commands the incident had disabled.

**It is not, however, why the 2026-08-01 incident has no postmortem.** An earlier draft of this document inferred that the tracker gap for that incident was caused by the same self-concealment. That inference was wrong. The operator has stated the actual reason: he did not instruct the coordinator to write one. Nothing was blocked; a postmortem was simply not requested. The gap has a human cause, not a systemic one, and reading a tooling failure into it would have manufactured evidence for a conclusion that does not need it.

The distinction matters for the same reason the rest of this document is careful about mechanisms: a self-concealing failure mode is worth stating because it is structurally true, not because it can be pinned to a specific missing record. §9.4's recommendations rest on the mechanism and on the eleven minutes of triage this outage actually cost — not on the Aug 1 gap.

### 9.4 What it should do instead

Startup must still fail hard. Every recommendation below preserves that.

1. **Log the fatal error before exiting.** Replace `main`'s bare propagation with an explicit `error!(error = ?err, "engine startup failed")` on the error path, so `/tmp/boss-engine.log` and the JSONL trace carry it alongside the audit record. Still returns `Err`, still exits non-zero. Nothing swallowed.
2. **Stop discarding stderr in both launchers.** `>>/tmp/boss-engine-stderr.log 2>&1` for the app's `nohup`; a `File` handle instead of `Stdio::null()` for the CLI. Nothing is retried or suppressed — the bytes are merely kept.
3. **Make the CLI report the cause.** `ensure_engine_running`'s bail should read the last `shutdown` record from `engine-audit.log` (or the stderr tail) and print it under the timeout: _"engine did not become ready … ; last engine exit: error:no such column: tmux_spawn_token in"_. This is a read of data that already exists and is the single highest-value item in this section.
4. **Make the app distinguish "engine never started" from "board is empty."** A caption-sized red dot is not adequate when the alternative reading is data loss. Surface the `gaveUp` supervision state as a persistent banner carrying the audit log's last shutdown reason and a "Reveal log" affordance. The state already exists and is already emitted (`emitSupervisionState`, `EngineProcessController.swift:856`); it simply is not rendered anywhere the operator looks.

**Explicitly not recommended,** per the incident brief and on the merits: swallowing schema errors, making bootstrap failures non-fatal, retry loops around bootstrap, hand-patching user databases as a supported recovery, or auto-recreating a database the engine cannot migrate. A database the engine cannot open is the operator's data. The only correct behaviour is to stop, leave it byte-for-byte intact, and say clearly why — which is what the engine did, and is the reason recovery cost one `ALTER TABLE` instead of a restore from backup.

## 10. Timeline

All times local.

| Time                     | Event                                                                                                                                                                                                                                                             |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 2026-08-01, before 23:27 | [#2634](https://github.com/spinyfin/mono/pull/2634) merges: typed status writes plus a `CHECK` constraint added by table-rebuild migration. **Incident 1 begins** — every engine start wedges, then boot-loops as the failed rebuild leaves `projects_v2` behind. |
| 2026-08-01 23:27         | [#2635](https://github.com/spinyfin/mono/pull/2635) (`c285efe1f4e9`) merges, tagged `boss-v1.0.486`. Fixes the rebuild under SQLite's 12-step procedure and adds 455 lines of old-database migration tests.                                                       |
| 2026-08-02 23:40         | Last successful engine bind on the operator's machine (`/tmp/boss-engine.sock` mtime). ~2.5 hours before the defect lands.                                                                                                                                        |
| 2026-08-03 02:11         | [#2642](https://github.com/spinyfin/mono/pull/2642) (`d15d0949a449`) merges: five tmux columns into the bootstrap `CREATE TABLE`, the unique index into the bootstrap batch, and a correct guarded migration 415 lines later.                                     |
| 2026-08-03 02:11–02:13   | Released as `boss-v1.0.489`. **~2 minutes from merge to a downloadable release.** Every install with history is now guaranteed to fail on next start.                                                                                                             |
| 2026-08-03 15:32         | [#2641](https://github.com/spinyfin/mono/pull/2641) (`boss-v1.0.494`) adds app-side engine restart supervision — which will convert the pending hard failure into a quiet retry-then-give-up.                                                                     |
| 2026-08-03 → 08-04       | Releases continue to `boss-v1.0.502`. The app auto-updates.                                                                                                                                                                                                       |
| 2026-08-06 12:36:25      | Operator launches Boss. Engine dies before bind. Crash loop begins; app shows "Disconnected" and an empty board.                                                                                                                                                  |
| 12:36–12:42              | 11 engine start failures. Each writes a complete `shutdown` diagnosis to `engine-audit.log`. Nothing reads it. `boss` / `bossctl` all fail with a bare 5-second timeout; `boss chore create` cannot file the incident.                                            |
| ~12:42–12:46             | Manual triage: stale socket ruled out; engine binary run by hand from the app bundle; real error obtained.                                                                                                                                                        |
| 12:46:33                 | Full backup taken (`state.db.bak-20260806-124633`, 128 MB).                                                                                                                                                                                                       |
| ~12:46                   | Operator-authorised `ALTER TABLE work_runs ADD COLUMN tmux_spawn_token TEXT`.                                                                                                                                                                                     |
| 12:47:06                 | Engine starts cleanly. Its own `migrate_work_runs_tmux_columns` appends the remaining four columns and the index. Outage ends.                                                                                                                                    |

## 11. Contributing factors

1. **The bootstrap batch conflates frozen history with live schema**, and nothing marks which it is.
2. **The written invariant is satisfied by the fatal statement.** "Idempotent against its own prior output" is true of `CREATE INDEX IF NOT EXISTS`; the governing rule — valid against the oldest supported database — is written nowhere.
3. **`CREATE TABLE IF NOT EXISTS` fails silently and adjacently.** It no-ops without warning, 25 lines above a statement that depends on its effect.
4. **The most authoritative-sounding test runs against a blank database**, so extending it produced a green result that could not have been red.
5. **No convention, no lint, no checklist** anywhere in the repo covering schema changes.
6. **Review cannot see the defect.** The diff is symmetric; the asymmetry is 400 lines of context away.
7. **Merge-to-release is ~2 minutes with one channel**, no soak, no canary, no in-development gate.
8. **Three independent stderr discards** put the cause out of reach of both entry points.
9. **A durable diagnosis is written and never read.** `engine-audit.log` had the answer 11 times.
10. **Restart supervision made a hard failure quiet** without giving the quiet failure a voice in the UI.
11. **The class recurred 26 hours after its own fix**, because that fix was scoped to its defect rather than to the class.

## 12. Action items

Owners are given by area. None of these are implemented by this document; the schema-ordering code fix is already tracked separately.

**Immediate — engine / schema**

1. Split the bootstrap batch so baseline DDL is tables only and all baseline index DDL runs after the full migration chain, over the already-migrated column set — making the baseline structurally incapable of failing on an existing database. Fold in the two indexes previously hand-hoisted out for this same reason, so there is one rule and no exceptions. Add structural guard tests: one asserting the baseline declares no indexes, one detecting baseline/migration column duplication by construction. _(Tracked separately — do not duplicate.)_
2. Remove the 40 duplicated column declarations from the baseline (§3.5), including the five `tmux_*` columns at `schema_init.rs:227-231`, so every column has a single owner. On the upgrade path the baseline copies are unreachable by design; on the fresh path the chain reaches the migration anyway, so the duplication buys nothing and costs the CI signal. _(Shipped as part of the same change as item 1 — tracked separately, do not duplicate.)_

**Immediate — engine / testing**

3. Add `migration_re_adds_tmux_columns_on_upgrade` to `schema_migration_tests.rs` (§7.1). Confirm it fails before the fix and passes after.

**Near-term — engine / testing + CI**

4. Add the generic upgrade guard of §7.2, with per-release text schema snapshots produced by `boss-release.sh` and one or two "fat" binary fixtures carrying representative rows including FK children.
5. Add a bundled-binary smoke start against the newest fat fixture to `mac-app-build`, so the release path validates the artifact that ships rather than a test binary.

**Near-term — engine / CLI + app / UX**

6. Log the fatal startup error through tracing before returning it (§9.4.1).
7. Stop redirecting engine stderr to `/dev/null` in both launchers (§9.4.2).
8. Make `ensure_engine_running`'s bail include the last `engine-audit.log` shutdown reason (§9.4.3). Highest value per line of change in this list.
9. Render the `gaveUp` supervision state as a persistent banner with the shutdown reason and a log affordance (§9.4.4).

**Structural — engine / schema + tooling**

10. Freeze the bootstrap batch and document the rule (§8.4). Record the invariant in `tools/boss/` guidance so the next author reads it without having to infer it.
11. Add the `checkleft` check that fails any diff touching the frozen batch and points at `migrations_*.rs` (§7.3).

**Process — release**

12. Decide whether in-development features warrant a release channel or soak period, given merge-to-auto-install is currently ~2 minutes. This is an operator decision, not an engineering default.
13. Adopt the two-part rule for schema-bearing in-development features (§8.3): behaviour behind a default-OFF registry flag; schema unconditionally safe and additive-only via a `migrate_*` function.

## 13. What went well

- **The engine failed closed and left the database pristine.** It died before binding, with zero writes. That is why recovery was one `ALTER TABLE` instead of a restore — and it is a direct contrast with the 2026-08-01 incident, where a missing transaction left a half-migrated database that failed _worse_ on every subsequent start. Fail-fast on schema bootstrap is correct behaviour and should not be softened.
- **The operator took a full backup before touching the live database.** Textbook, and it made the manual remediation a reversible decision rather than a gamble.
- **The engine's own migration completed the repair.** The hand-written `ALTER` only unblocked the batch; the guarded migration did the rest correctly, including skipping the column already added. The mechanism works.
- **Triage was efficient and correctly reasoned.** Ruling out the stale socket by observing that the engine did not recreate it established "dying before bind" quickly and correctly, without red herrings.
- **The forensic groundwork paid off partially.** `record_start` captures `prior_state_db_size`, `record_shutdown` captures the reason, and `record_socket_bind` exists specifically because a 2026-05-07 incident left the bind window invisible. The data was all there. Only the last step — a client that reads it — is missing.
- **The material to prevent this was already in the tree**, two days old: the upgrade-test pattern from [#2635](https://github.com/spinyfin/mono/pull/2635) and the flag system from incident 001.

## 14. What went badly

- A schema change bricked every install with history for three days before anyone noticed, because nobody with such an install restarted the engine in that window.
- Diagnosis required running a binary out of an app bundle by hand, while the exact answer sat in a documented log file, eleven times over.
- The outage blocked the command that would have recorded it.
- The recurrence came 26 hours after a fix for the same class, using none of the tooling that fix had just built.
- Restart supervision, landed in good faith between the two incidents, made the second one quieter than the first.

## 15. Lessons

1. **"Correct against a blank database" is not correctness.** The only database that matters is one with history. A test suite that never opens one is not testing schema; it is testing DDL syntax.
2. **A migration that exists is not a migration that runs.** Reachability is part of the change, and it is invisible in a diff.
3. **Idempotence is not sufficiency.** `IF NOT EXISTS` makes a statement safe to _repeat_. It says nothing about whether the statement is safe to _run_.
4. **Inert code is not inert when it is schema.** "Not wired up yet" is an argument for more care with DDL, not less, because DDL runs before every gate.
5. **Flags gate behaviour, not bootstrap.** Feature-flagging the tmux spawn path is still right and still worth doing — and it would not have prevented this outage. Say both things.
6. **A fix scoped to a defect does not stop a class.** The 2026-08-01 fix was specific, well-reasoned, and left the door it came through open.
7. **A defect that blocks engine start erases its own record.** That makes startup observability a durability property of the incident process itself, not a nicety.
8. **Recording a diagnosis is not surfacing it.** Boss wrote down exactly what was wrong, in a known file, by design, eleven times — and it cost eleven minutes anyway, because no client reads it.

## 16. Follow-up code changes

This document is doc-only. The changes it recommends are enumerated in §12 and should be filed as separate chores against the project that owns the engine's persistence layer. The schema-ordering fix itself (items 1–2) is already tracked as its own change and is deliberately not duplicated here.
