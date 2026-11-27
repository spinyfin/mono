use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use boss_acp_client::schema::RequestId;
use boss_agent_host::journal::{Direction, Journal};
use serde_json::json;

use crate::intent::{IntentLog, read_intent_file};
use crate::queue::PromptOrigin;
use crate::redact::CredentialRedactor;
use crate::reducer::{DEFAULT_TOOL_TEXT_LIMIT, replay_with_intent_log};
use crate::retention::{
    ASSUMED_MAX_TURNS_PER_DAY, DEFAULT_MAX_AGE, DEFAULT_MAX_TOTAL_BYTES, Generation, GenerationStore, PruneReason,
    RetentionPolicy,
};
use crate::snapshot::{read_wire_file_if_exists, save_snapshot};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

fn store() -> (tempfile::TempDir, GenerationStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = GenerationStore::new(dir.path().join("chat"));
    (dir, store)
}

/// A generation holding `bytes` bytes, last written at `written`.
fn generation_written_at(store: &GenerationStore, bytes: usize, written: SystemTime) -> Generation {
    let generation = store.create().unwrap();
    fs::write(generation.wire_path(), vec![b'x'; bytes]).unwrap();
    set_mtimes(generation.dir(), written);
    generation
}

fn set_mtimes(dir: &Path, time: SystemTime) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
    }
    File::open(dir).unwrap().set_modified(time).unwrap();
}

fn ids(report: &crate::retention::PruneReport) -> Vec<&str> {
    report.removed.iter().map(|pruned| pruned.id.as_str()).collect()
}

#[test]
fn a_generation_directory_is_private_and_can_be_reopened_by_id() {
    let (_dir, store) = store();
    let generation = store.create().unwrap();
    let mode = fs::metadata(generation.dir()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert_eq!(generation.wire_path(), generation.dir().join("wire.jsonl"));
    assert_eq!(generation.intent_path(), generation.dir().join("intent.jsonl"));
    assert_eq!(generation.snapshot_path(), generation.dir().join("snapshot.json"));
    assert_eq!(generation.images_dir(), generation.dir().join("images"));
    assert_eq!(store.open(generation.id()).unwrap(), generation);
}

#[test]
fn generation_ids_are_unique_and_sort_by_creation() {
    let (_dir, store) = store();
    let ids: Vec<String> = (0..50).map(|_| store.create().unwrap().id().to_owned()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, ids, "every id differs and creation order is sort order");
}

#[test]
fn only_a_generation_id_opens_a_generation() {
    let (_dir, store) = store();
    let generation = store.create().unwrap();
    for bad in [
        "",
        ".",
        "..",
        "../chat",
        "a/b",
        "1795720000000",
        "1795720000000-x-0",
        "1795720000000-1-",
    ] {
        assert!(store.open(bad).is_err(), "{bad:?} must not open");
    }
    fs::create_dir(store.root().join("not-a-generation")).unwrap();
    assert!(store.open("not-a-generation").is_err());
    // A well-formed id with no directory.
    let missing = format!("{}-ffff-00000000", "9".repeat(13));
    assert!(store.open(&missing).is_err());
    assert!(store.open(generation.id()).is_ok());
}

#[test]
fn pruning_a_root_that_does_not_exist_does_nothing() {
    let (_dir, store) = store();
    let report = store
        .prune("live", &RetentionPolicy::default(), SystemTime::now())
        .unwrap();
    assert!(report.removed.is_empty() && report.failed.is_empty() && report.kept == 0);
}

#[test]
fn the_live_generation_is_never_pruned_however_old_or_large() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let live = generation_written_at(&store, 4_000, now - 90 * DAY);
    let old = generation_written_at(&store, 4_000, now - 90 * DAY);
    let policy = RetentionPolicy {
        max_age: DAY,
        max_total_bytes: 0,
    };
    let report = store.prune(live.id(), &policy, now).unwrap();
    assert_eq!(ids(&report), [old.id()]);
    assert!(live.dir().exists() && !old.dir().exists());
}

#[test]
fn starting_a_generation_prunes_the_old_ones_and_keeps_the_new_one() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let stale = generation_written_at(&store, 100, now - 30 * DAY);
    let recent = generation_written_at(&store, 100, now - DAY);
    let (started, report) = store.create_and_prune(&RetentionPolicy::default()).unwrap();
    assert_eq!(ids(&report), [stale.id()]);
    assert!(recent.dir().exists() && started.dir().exists());
    assert!(started.id() > recent.id(), "the new generation sorts last");
}

#[test]
fn a_generation_older_than_the_max_age_is_removed_and_a_recent_one_is_kept() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let stale = generation_written_at(&store, 100, now - 15 * DAY);
    let fresh = generation_written_at(&store, 100, now - 13 * DAY);
    let report = store.prune("live", &RetentionPolicy::default(), now).unwrap();
    assert_eq!(ids(&report), [stale.id()]);
    assert_eq!(report.removed[0].reason, PruneReason::Age);
    assert!(fresh.dir().exists());
    assert_eq!((report.kept, report.kept_bytes), (1, 100));
}

#[test]
fn age_is_judged_by_the_newest_file_in_the_generation() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let generation = generation_written_at(&store, 100, now - 30 * DAY);
    // The snapshot is written last when a generation winds down.
    let snapshot = generation.snapshot_path();
    fs::write(&snapshot, b"{}").unwrap();
    File::options()
        .write(true)
        .open(&snapshot)
        .unwrap()
        .set_modified(now - DAY)
        .unwrap();
    let report = store.prune("live", &RetentionPolicy::default(), now).unwrap();
    assert!(
        report.removed.is_empty(),
        "a recently written file keeps the generation"
    );
}

#[test]
fn over_the_size_cap_the_oldest_generations_go_first() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let oldest = generation_written_at(&store, 1_000, now - 5 * DAY);
    let middle = generation_written_at(&store, 1_000, now - 3 * DAY);
    let newest = generation_written_at(&store, 1_000, now - DAY);
    let policy = RetentionPolicy {
        max_age: 30 * DAY,
        max_total_bytes: 2_500,
    };
    let report = store.prune("live", &policy, now).unwrap();
    assert_eq!(ids(&report), [oldest.id()]);
    assert_eq!(report.removed[0].reason, PruneReason::Size);
    assert!(middle.dir().exists() && newest.dir().exists());
    assert_eq!(report.kept_bytes, 2_000);

    // Still over the cap: another pass takes the next oldest.
    let policy = RetentionPolicy {
        max_age: 30 * DAY,
        max_total_bytes: 1_500,
    };
    let report = store.prune("live", &policy, now).unwrap();
    assert_eq!(ids(&report), [middle.id()]);
}

#[test]
fn the_size_rule_counts_only_what_the_age_rule_left() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let stale = generation_written_at(&store, 10_000, now - 20 * DAY);
    let kept = generation_written_at(&store, 1_000, now - DAY);
    let policy = RetentionPolicy {
        max_age: 14 * DAY,
        max_total_bytes: 5_000,
    };
    let report = store.prune("live", &policy, now).unwrap();
    assert_eq!(
        ids(&report),
        [stale.id()],
        "the old one went by age; the cap is then met"
    );
    assert!(kept.dir().exists());
}

#[test]
fn nothing_that_is_not_a_generation_directory_is_touched() {
    let (_dir, store) = store();
    let now = SystemTime::now();
    let stale = generation_written_at(&store, 100, now - 90 * DAY);
    fs::create_dir(store.root().join("somebody-elses-notes")).unwrap();
    fs::write(store.root().join("somebody-elses-notes").join("n.txt"), b"keep").unwrap();
    fs::write(
        store.root().join("1795720000000-1-00000000"),
        b"a file named like a generation",
    )
    .unwrap();
    let policy = RetentionPolicy {
        max_age: Duration::ZERO,
        max_total_bytes: 0,
    };
    let report = store.prune("live", &policy, now + DAY).unwrap();
    assert_eq!(ids(&report), [stale.id()]);
    assert!(store.root().join("somebody-elses-notes/n.txt").exists());
    assert!(store.root().join("1795720000000-1-00000000").exists());
}

#[test]
fn a_symbolic_link_inside_a_generation_is_removed_and_not_followed() {
    let (dir, store) = store();
    let now = SystemTime::now();
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious.txt"), vec![b'p'; 50_000]).unwrap();
    let stale = generation_written_at(&store, 100, now - 90 * DAY);
    fs::create_dir(stale.images_dir()).unwrap();
    std::os::unix::fs::symlink(&outside, stale.images_dir().join("link")).unwrap();
    set_mtimes(stale.dir(), now - 90 * DAY);
    // A link's own modification time cannot be set from here, so judge the
    // generation by a clock far enough ahead that even a new link is old.
    let report = store
        .prune("live", &RetentionPolicy::default(), now + 120 * DAY)
        .unwrap();
    assert_eq!(ids(&report), [stale.id()]);
    assert!(report.removed[0].bytes < 50_000, "the link target was not counted");
    assert!(outside.join("precious.txt").exists(), "the link target was not removed");
}

// ---- the measurement behind the defaults ------------------------------------

/// One turn of spike-shaped traffic: the operator's prompt, a streamed answer
/// in deltas of about 15 characters, two tool calls with fenced output, a
/// usage update, and the prompt's response. Returns the next request id.
fn append_turn(journal: &mut Journal, intents: &mut IntentLog, turn: usize, request_id: i64, session: &str) {
    let origin = if turn.is_multiple_of(25) {
        PromptOrigin::Engine
    } else {
        PromptOrigin::Operator
    };
    let queued = intents
        .append_queued_at(
            1_795_720_000_000 + turn as u64 * 2_000,
            origin,
            format!("Turn {turn}: what is the status of the open work items for the coordinator panel project?"),
            (origin == PromptOrigin::Operator).then(|| format!("cm-{turn}")),
            Vec::new(),
        )
        .unwrap();
    intents
        .append_sent_at(queued.ts_ms + 1, queued.intent_id(), RequestId::Number(request_id))
        .unwrap();
    let line = |value: serde_json::Value| value.to_string();
    journal
        .append(
            Direction::ToAgent,
            &line(json!({
                "jsonrpc": "2.0", "id": request_id, "method": "session/prompt",
                "params": {"sessionId": session, "prompt": [{"type": "text", "text":
                    format!("Turn {turn}: what is the status of the open work items for the coordinator panel project?")}]},
            })),
        )
        .unwrap();
    let update = |update: serde_json::Value| {
        line(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}}))
    };
    for chunk in 0..18 {
        journal
            .append(
                Direction::FromAgent,
                &update(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": format!("chunk {chunk:02} of text ")},
                    "messageId": format!("msg-{turn}"),
                })),
            )
            .unwrap();
    }
    for call in 0..2 {
        let tool_call_id = format!("toolu_{turn:04}_{call}");
        journal
            .append(
                Direction::FromAgent,
                &update(json!({
                    "sessionUpdate": "tool_call", "toolCallId": tool_call_id,
                    "title": "boss task list --product coordinator-panel --json --no-input",
                    "kind": "execute", "status": "pending",
                    "rawInput": {"command": "boss task list --product coordinator-panel --json --no-input",
                                 "description": "List the tasks of the coordinator panel product"},
                })),
            )
            .unwrap();
        journal
            .append(
                Direction::FromAgent,
                &update(json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": tool_call_id, "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text",
                        "text": format!("```console\n{}\n```", "task-123 Engine chat service  running  worker-3\n".repeat(14))}}],
                })),
            )
            .unwrap();
    }
    journal
        .append(
            Direction::FromAgent,
            &update(json!({"sessionUpdate": "usage_update", "used": 42_604 + turn, "size": 200_000})),
        )
        .unwrap();
    journal
        .append(
            Direction::FromAgent,
            &line(json!({"jsonrpc": "2.0", "id": request_id, "result": {"stopReason": "end_turn"}})),
        )
        .unwrap();
}

/// What a generation of `turns` turns costs on disk, written by the real
/// writers: the agent host's journal, the engine's intent log, and the
/// reducer snapshot.
struct DiskCost {
    wire: u64,
    intent: u64,
    snapshot: u64,
}

impl DiskCost {
    fn total(&self) -> u64 {
        self.wire + self.intent + self.snapshot
    }
}

fn write_synthetic_generation(generation: &Generation, turns: usize) -> DiskCost {
    let session = "0a4f6c1e-5d2b-4c7e-9a3f-1b8d7e6c5a42";
    let mut journal = Journal::create(&generation.wire_path()).unwrap();
    let mut intents = IntentLog::open(&generation.intent_path()).unwrap();
    let line = |value: serde_json::Value| value.to_string();
    journal
        .append(
            Direction::ToAgent,
            &line(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1}})),
        )
        .unwrap();
    journal
        .append(
            Direction::FromAgent,
            &line(json!({"jsonrpc": "2.0", "id": 1, "result": {
                "protocolVersion": 1,
                "agentInfo": {"name": "@agentclientprotocol/claude-agent-acp", "version": "0.110.0"},
                "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks", "backgroundSubagents"]}}},
            }})),
        )
        .unwrap();
    journal
        .append(
            Direction::ToAgent,
            &line(json!({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": "/session", "mcpServers": []}})),
        )
        .unwrap();
    journal
        .append(
            Direction::FromAgent,
            &line(json!({"jsonrpc": "2.0", "id": 2, "result": {
                "sessionId": session, "modes": {"currentModeId": "auto"},
                "configOptions": [{"id": "model", "currentValue": "opus"}],
            }})),
        )
        .unwrap();
    // The spike's largest line, 27,587 bytes, which the adapter re-sends as the
    // session's commands change.
    let commands: Vec<_> = (0..71)
        .map(
            |n| json!({"name": format!("command_{n:02}"), "description": "d".repeat(360), "input": {"hint": "<args>"}}),
        )
        .collect();
    let commands_line = line(json!({"jsonrpc": "2.0", "method": "session/update", "params": {
        "sessionId": session, "update": {"sessionUpdate": "available_commands_update", "availableCommands": commands}}}));
    assert!(commands_line.len() >= 27_587);
    journal.append(Direction::FromAgent, &commands_line).unwrap();
    for turn in 0..turns {
        if turn > 0 && turn.is_multiple_of(50) {
            journal.append(Direction::FromAgent, &commands_line).unwrap();
        }
        append_turn(&mut journal, &mut intents, turn, turn as i64 + 3, session);
    }
    drop(journal);
    drop(intents);

    let wire = read_wire_file_if_exists(&generation.wire_path()).unwrap();
    let intent = read_intent_file(&generation.intent_path()).unwrap();
    let reducer = replay_with_intent_log(&wire, &intent, CredentialRedactor::none(), DEFAULT_TOOL_TEXT_LIMIT);
    assert!(
        reducer.items().len() > turns * 4,
        "the synthetic traffic reduces to a real conversation: {} items",
        reducer.items().len()
    );
    save_snapshot(&generation.snapshot_path(), &reducer.snapshot()).unwrap();
    let size = |path: std::path::PathBuf| fs::metadata(path).unwrap().len();
    DiskCost {
        wire: size(generation.wire_path()),
        intent: size(generation.intent_path()),
        snapshot: size(generation.snapshot_path()),
    }
}

/// The measurement the default policy is set from. Run it with `--nocapture`
/// to read the `RETENTION_MEASURE` line; the numbers in the module docs of
/// `retention` are from it.
#[test]
fn measure_generation_disk_cost() {
    let (_dir, store) = store();
    let generation = store.create().unwrap();
    let turns = 250;
    let cost = write_synthetic_generation(&generation, turns);
    let per_turn = cost.total() / turns as u64;
    eprintln!(
        "RETENTION_MEASURE: turns={turns} wire_bytes={} intent_bytes={} snapshot_bytes={} total_bytes={} per_turn_bytes={per_turn} \
         wire_per_turn={} intent_per_turn={} snapshot_per_turn={}",
        cost.wire,
        cost.intent,
        cost.snapshot,
        cost.total(),
        cost.wire / turns as u64,
        cost.intent / turns as u64,
        cost.snapshot / turns as u64,
    );

    // The size cap must not cut in before the age rule at the assumed ceiling
    // turn rate, or it would be the reason directories go in ordinary use
    // instead of the bound for a runaway one.
    let days = DEFAULT_MAX_AGE.as_secs() / DAY.as_secs();
    let worst_case_non_live_bytes = per_turn * ASSUMED_MAX_TURNS_PER_DAY * days;
    assert!(
        worst_case_non_live_bytes <= DEFAULT_MAX_TOTAL_BYTES,
        "{days} days at {ASSUMED_MAX_TURNS_PER_DAY} turns a day costs {worst_case_non_live_bytes} bytes at the \
         measured {per_turn} bytes a turn, over the {DEFAULT_MAX_TOTAL_BYTES}-byte cap"
    );
    // And the cap is a bound, not an invitation: not so far above that rate
    // that a runaway generation could fill a disk before it applies.
    assert!(DEFAULT_MAX_TOTAL_BYTES < 2 * worst_case_non_live_bytes);
}
