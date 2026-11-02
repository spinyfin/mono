# Engine database migrations

Engine startup owns migration execution in `tools/boss/engine/work-db/src/schema_init.rs`. It takes one `BEGIN IMMEDIATE` transaction covering baseline creation, legacy bootstrap, every pending migration, and required local-host data. An error rolls the whole transaction back. CLI inspection and direct attachment do not run migrations.

## Adding a migration

Add a separate `Migration` entry in `tools/boss/engine/work-db/src/schema_migration_registry.rs`, with a unique `YYYYMMDD_HHMM_short_snake_name` id using UTC and `legacy_version: None`. The descriptive suffix distinguishes migrations authored in the same minute. Put substantial SQL or Rust transformation logic in its own file or the existing owning module, then call it from the entry. Do not edit a shared version counter, renumber existing entries, or modify a shipped migration's id or behavior.

The runner sorts entries by id, independently of their source order, and applies every known id absent from `schema_migrations`. An earlier timestamp that merges later is still applied; this is set membership, not a high-water mark. Duplicate registry ids fail startup before any migration runs.

**Migrations from parallel PRs must not depend on one another.** They must work regardless of which PR is deployed first. If one migration needs another migration's tables, columns, or transformed data, wait for that PR to merge, rebase, then author the dependent migration with a later UTC id. Chronological sorting cannot substitute for that sequencing rule.

## Applied history and compatibility

`schema_migrations(id TEXT PRIMARY KEY NOT NULL, applied_at TEXT NOT NULL)` records successful migrations with UTC timestamps. The timestamp records application or bootstrap time, not the original deployment time of historical changes. A database containing any applied id unknown to the binary refuses startup with the unknown ids and an instruction to upgrade the engine. Unknown entries are never ignored or deleted automatically.

The compatibility floor remains Boss 1.0.707, integer schema 32. Databases older than that release, or incomplete databases claiming its schema, remain unsupported. Fresh databases install that released baseline and then run all later migrations.

For an existing database without a ledger, startup validates its integer version and seeds all historical ids through that version without executing those migrations again. Frozen ids `00000000_0000_vNNN_name` preserve the integer order: versions 1–32 identify the already-squashed baseline, version 57 had no separate schema step, and the remaining entries preserve the historical upgrade operations through version 60. Unknown higher integer versions are rejected. New migrations never extend the integer history.

Bootstrap removes `metadata.schema_version` in the same transaction. The ledger is authoritative on subsequent opens. Removing the old marker also makes the existing integer compatibility-floor check reject the database as unversioned instead of trusting a stale integer version. Downgrading to an integer-version engine requires restoring a compatible database backup along with the older binary.

## Validation

Run `bazel build //tools/boss/engine/core:engine_lib` and `bazel test //tools/boss/engine/...`, followed by `checkleft run`. Migration tests cover fresh creation, legacy upgrades from the released schema golden, bootstrap without replay, independent migration order, earlier ids arriving later, duplicate and unknown ids, and transaction rollback.

Upgrade fixtures representing an integer-era database must have no `schema_migrations` table and must set `metadata.schema_version` to the fixture's historical version. Fixtures starting from a current database must explicitly drop the ledger when reconstructing that old state. Named-era fixtures should remove only the specific migration ids whose effects they undo. Keep the released baseline golden independent of the implementation under test.
