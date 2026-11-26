//! The host end to end: a real child (the scripted one), a real socket and
//! real clients that die and come back.
//!
//! Paths to the helper binaries come from the environment because Bazel
//! stages them in the test's runfiles (see this package's `BUILD.bazel`).

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use boss_agent_host::journal::{Direction, WireRecord, read_records};
use boss_agent_host::{Config, Handshake, run};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::task::JoinHandle;

const STEP_TIMEOUT: Duration = Duration::from_secs(20);

fn helper(var: &str) -> PathBuf {
    PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var} is not set; run this under bazel test")))
}

fn script_line(dir: Direction, line: &str) -> String {
    // A journal record, as the host writes it, so the helper is shown to
    // accept a recorded journal unedited.
    let record = WireRecord {
        seq: 0,
        ts_ms: 0,
        dir,
        line: line.to_owned(),
        invalid: None,
    };
    serde_json::to_string(&record).unwrap()
}

fn from_agent(line: &str) -> String {
    script_line(Direction::FromAgent, line)
}

fn to_agent(line: &str) -> String {
    script_line(Direction::ToAgent, line)
}

struct Host {
    // Held so the directory outlives the host. The test sandbox's temp dir
    // is short enough for a Unix socket path (about 100 bytes at most).
    _dir: tempfile::TempDir,
    socket: PathBuf,
    journal: PathBuf,
    task: JoinHandle<io::Result<i32>>,
}

impl Host {
    async fn start(script: &[String], child_args: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("script.jsonl");
        std::fs::write(&script_path, script.join("\n") + "\n").unwrap();
        let socket = dir.path().join("h.sock");
        let journal = dir.path().join("wire.jsonl");
        let mut command = vec![
            helper("BOSS_AGENT_HOST_TEST_CHILD").into_os_string(),
            script_path.into_os_string(),
        ];
        command.extend(child_args.iter().map(Into::into));
        let task = tokio::spawn(run(Config {
            socket_path: socket.clone(),
            journal_path: journal.clone(),
            command,
        }));
        wait_for_socket(&socket).await;
        Self {
            _dir: dir,
            socket,
            journal,
            task,
        }
    }

    async fn connect(&self, replay_from: u64) -> Client {
        Client::connect(&self.socket, replay_from).await
    }

    async fn finish(self) -> i32 {
        tokio::time::timeout(STEP_TIMEOUT, self.task)
            .await
            .expect("the host exits once the child has")
            .unwrap()
            .unwrap()
    }

    fn journal_records(&self) -> Vec<WireRecord> {
        read_records(std::fs::File::open(&self.journal).map(std::io::BufReader::new).unwrap()).unwrap()
    }

    async fn wait_for_journal_len(&self, len: usize) {
        tokio::time::timeout(STEP_TIMEOUT, async {
            while self.journal_records().len() < len {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the journal reaches the expected length");
    }
}

async fn wait_for_socket(socket: &Path) {
    tokio::time::timeout(STEP_TIMEOUT, async {
        while UnixStream::connect(socket).await.is_err() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the host starts listening");
}

struct Client {
    read: tokio::io::Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

impl Client {
    async fn connect(socket: &Path, replay_from: u64) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{}\n", Handshake { replay_from }.line()).as_bytes())
            .await
            .unwrap();
        Self {
            read: BufReader::new(read).lines(),
            write,
        }
    }

    /// The next record, or `None` when the host closed the connection.
    async fn next(&mut self) -> Option<WireRecord> {
        let line = tokio::time::timeout(STEP_TIMEOUT, self.read.next_line())
            .await
            .expect("the host sends a record or closes")
            .ok()??;
        Some(serde_json::from_str(&line).expect("the host sends journal records"))
    }

    async fn expect(&mut self, seq: u64, dir: Direction, line: &str) {
        let record = self
            .next()
            .await
            .unwrap_or_else(|| panic!("connection closed before record {seq}"));
        assert_eq!((record.seq, record.dir, record.line.as_str()), (seq, dir, line));
    }

    async fn send(&mut self, line: &str) {
        self.write.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }

    async fn collect_until_closed(&mut self) -> Vec<WireRecord> {
        let mut records = Vec::new();
        while let Some(record) = self.next().await {
            records.push(record);
        }
        records
    }
}

#[tokio::test]
async fn relays_both_directions_and_journals_every_line() {
    let host = Host::start(&[from_agent("hello"), to_agent("ping"), from_agent("pong")], &[]).await;
    let mut client = host.connect(1).await;

    client.expect(1, Direction::FromAgent, "hello").await;
    client.send("ping").await;
    // The client's own line comes back, in order, before the reply it caused.
    client.expect(2, Direction::ToAgent, "ping").await;
    client.expect(3, Direction::FromAgent, "pong").await;
    assert!(client.next().await.is_none(), "the host closes when the child exits");

    let records = host.journal_records();
    assert_eq!(records.iter().map(|r| r.seq).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(
        records.iter().map(|r| (r.dir, r.line.as_str())).collect::<Vec<_>>(),
        [
            (Direction::FromAgent, "hello"),
            (Direction::ToAgent, "ping"),
            (Direction::FromAgent, "pong"),
        ]
    );
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn output_written_before_any_client_connects_is_replayed() {
    let host = Host::start(
        &[from_agent("a"), from_agent("b"), from_agent("c"), to_agent("go")],
        &[],
    )
    .await;
    host.wait_for_journal_len(3).await;

    let mut client = host.connect(1).await;
    client.expect(1, Direction::FromAgent, "a").await;
    client.expect(2, Direction::FromAgent, "b").await;
    client.expect(3, Direction::FromAgent, "c").await;
    client.send("go").await;
    client.expect(4, Direction::ToAgent, "go").await;
    assert!(client.next().await.is_none());
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn replay_starts_at_the_requested_sequence_number() {
    let host = Host::start(
        &[from_agent("a"), from_agent("b"), from_agent("c"), to_agent("go")],
        &[],
    )
    .await;
    host.wait_for_journal_len(3).await;

    let mut from_second = host.connect(2).await;
    from_second.expect(2, Direction::FromAgent, "b").await;
    from_second.expect(3, Direction::FromAgent, "c").await;

    // Past the end: nothing is replayed, and the next thing is live.
    let mut past_end = host.connect(99).await;
    past_end.send("go").await;
    past_end.expect(4, Direction::ToAgent, "go").await;
    assert!(past_end.next().await.is_none());
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn a_client_killed_mid_stream_loses_and_repeats_nothing_on_reconnect() {
    const LINES: u64 = 300;
    let mut script: Vec<String> = (1..=LINES).map(|n| from_agent(&format!("line {n}"))).collect();
    script.push(to_agent("done"));
    // A small delay keeps the child producing while the clients come and go,
    // so the reconnect lands in the middle of the stream.
    let host = Host::start(&script, &["--delay-ms", "2"]).await;

    let mut first = host.connect(1).await;
    let mut seen = Vec::new();
    for _ in 0..40 {
        seen.push(first.next().await.expect("records while the child is running"));
    }
    // Killed: the socket goes away with unread data on it.
    drop(first);

    let resume_at = seen.last().unwrap().seq + 1;
    let mut second = host.connect(resume_at).await;
    while seen.last().unwrap().seq < LINES {
        seen.push(second.next().await.expect("records while the child is running"));
    }
    second.send("done").await;
    seen.extend(second.collect_until_closed().await);

    let seqs: Vec<u64> = seen.iter().map(|r| r.seq).collect();
    assert_eq!(
        seqs,
        (1..=LINES + 1).collect::<Vec<_>>(),
        "contiguous, no gap, no repeat"
    );
    for record in &seen[..LINES as usize] {
        assert_eq!(record.dir, Direction::FromAgent);
        assert_eq!(record.line, format!("line {}", record.seq));
    }
    assert_eq!(
        (seen[LINES as usize].dir, seen[LINES as usize].line.as_str()),
        (Direction::ToAgent, "done")
    );
    assert_eq!(seen, host.journal_records(), "what the client saw is the journal");
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn a_new_connection_replaces_the_older_one() {
    let host = Host::start(&[from_agent("one"), to_agent("go"), from_agent("two")], &[]).await;
    host.wait_for_journal_len(1).await;

    let mut older = host.connect(1).await;
    older.expect(1, Direction::FromAgent, "one").await;

    let mut newer = host.connect(1).await;
    newer.expect(1, Direction::FromAgent, "one").await;
    // The older connection is closed by the host, with nothing more to read.
    assert!(older.next().await.is_none());

    newer.send("go").await;
    newer.expect(2, Direction::ToAgent, "go").await;
    newer.expect(3, Direction::FromAgent, "two").await;
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn a_connection_that_never_completes_a_handshake_displaces_nobody() {
    let host = Host::start(&[from_agent("one"), to_agent("go"), from_agent("two")], &[]).await;
    let mut client = host.connect(1).await;
    client.expect(1, Direction::FromAgent, "one").await;

    // Silent: connected, nothing sent.
    let _silent = UnixStream::connect(&host.socket).await.unwrap();
    // Malformed: closed by the host.
    let mut malformed = UnixStream::connect(&host.socket).await.unwrap();
    malformed.write_all(b"not a handshake\n").await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(
        STEP_TIMEOUT,
        tokio::io::AsyncReadExt::read_to_end(&mut malformed, &mut rest),
    )
    .await
    .expect("the host closes a malformed connection")
    .unwrap();
    assert!(rest.is_empty());

    client.send("go").await;
    client.expect(2, Direction::ToAgent, "go").await;
    client.expect(3, Direction::FromAgent, "two").await;
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn a_line_cut_off_by_the_client_dying_is_not_forwarded() {
    let host = Host::start(&[from_agent("ready"), to_agent("whole line"), from_agent("done")], &[]).await;
    let mut dying = host.connect(1).await;
    dying.expect(1, Direction::FromAgent, "ready").await;
    dying.write.write_all(b"whole li").await.unwrap();
    drop(dying);

    let mut next = host.connect(2).await;
    next.send("whole line").await;
    // Had the fragment been forwarded, the child would have read
    // "whole liwhole line" and exited 3 instead of finishing the script.
    next.expect(2, Direction::ToAgent, "whole line").await;
    next.expect(3, Direction::FromAgent, "done").await;
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn output_written_just_before_the_child_exits_still_reaches_the_client() {
    let host = Host::start(
        &[from_agent("a"), to_agent("go"), from_agent("b"), from_agent("c")],
        &["--exit-code", "5"],
    )
    .await;
    let mut client = host.connect(1).await;
    client.expect(1, Direction::FromAgent, "a").await;
    client.send("go").await;
    let rest = client.collect_until_closed().await;
    assert_eq!(
        rest.iter().map(|r| (r.seq, r.line.as_str())).collect::<Vec<_>>(),
        [(2, "go"), (3, "b"), (4, "c")]
    );
    assert_eq!(host.finish().await, 5, "the host exits with the child's status");
}

#[tokio::test]
async fn a_second_host_does_not_take_a_live_hosts_socket_or_touch_its_journal() {
    let host = Host::start(&[from_agent("one"), to_agent("go")], &[]).await;
    host.wait_for_journal_len(1).await;
    let before = std::fs::read(&host.journal).unwrap();

    // The refused host is pointed at the live host's own journal.
    let result = run(Config {
        socket_path: host.socket.clone(),
        journal_path: host.journal.clone(),
        command: vec![helper("BOSS_AGENT_HOST_TEST_CHILD").into_os_string()],
    })
    .await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AddrInUse);
    assert_eq!(
        std::fs::read(&host.journal).unwrap(),
        before,
        "the journal is untouched"
    );

    // The live host is untouched: it replays what it had and keeps appending.
    let mut client = host.connect(1).await;
    client.expect(1, Direction::FromAgent, "one").await;
    client.send("go").await;
    client.expect(2, Direction::ToAgent, "go").await;
    assert_eq!(host.journal_records().len(), 2);
    assert_eq!(host.finish().await, 0);
}

#[tokio::test]
async fn replacing_a_client_while_a_large_line_is_unread_by_the_child_keeps_lines_whole() {
    // Far past a pipe buffer, and the child does not read for a while, so the
    // write to its stdin is still pending when the client is replaced.
    let big = "x".repeat(1 << 20);
    let host = Host::start(
        &[
            from_agent("ready"),
            to_agent(&big),
            to_agent("small"),
            from_agent("done"),
        ],
        &["--stdin-delay-ms", "700"],
    )
    .await;
    let mut first = host.connect(1).await;
    first.expect(1, Direction::FromAgent, "ready").await;
    first.send(&big).await;
    host.wait_for_journal_len(2).await;

    let mut second = host.connect(1).await;
    second.expect(1, Direction::FromAgent, "ready").await;
    let record = second.next().await.unwrap();
    assert_eq!(
        (record.seq, record.dir, record.line.len()),
        (2, Direction::ToAgent, big.len())
    );
    second.send("small").await;
    // The scripted child exits 3 on any line that is not exactly the script's.
    second.expect(3, Direction::ToAgent, "small").await;
    second.expect(4, Direction::FromAgent, "done").await;
    assert!(second.next().await.is_none());
    assert_eq!(host.finish().await, 0, "the child read both lines whole");
}

#[tokio::test]
async fn a_termination_signal_reaches_the_child_before_it_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let cleaned = dir.path().join("cleaned");
    let mut host = tokio::process::Command::new(helper("BOSS_AGENT_HOST_TEST_BIN"))
        .arg("--socket")
        .arg(dir.path().join("h.sock"))
        .arg("--journal")
        .arg(dir.path().join("wire.jsonl"))
        .arg("--")
        .args(["/bin/sh", "-c"])
        .arg(format!(
            "trap 'touch {cleaned:?}; exit 0' TERM; touch {ready:?}; while :; do sleep 0.1; done"
        ))
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(STEP_TIMEOUT, async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the child installs its handler");

    // SAFETY: signalling a child process this test just spawned.
    unsafe {
        libc::kill(host.id().unwrap() as i32, libc::SIGTERM);
    }
    let status = tokio::time::timeout(STEP_TIMEOUT, host.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    assert!(cleaned.exists(), "the child's own handler ran");
}

#[tokio::test]
async fn a_socket_file_left_by_a_dead_host_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("h.sock");
    let script = dir.path().join("script.jsonl");
    std::fs::write(&script, to_agent("go") + "\n").unwrap();

    // A real dead host: killed outright, so it cannot clean up its socket.
    // (A listener made in this process and dropped is not a reliable stand-in:
    // on macOS a concurrent spawn can inherit its descriptor and keep it live.)
    let mut doomed = tokio::process::Command::new(helper("BOSS_AGENT_HOST_TEST_BIN"))
        .arg("--socket")
        .arg(&socket)
        .arg("--journal")
        .arg(dir.path().join("old.jsonl"))
        .arg("--")
        .arg(helper("BOSS_AGENT_HOST_TEST_CHILD"))
        .arg(&script)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_socket(&socket).await;
    doomed.kill().await.unwrap();
    assert!(socket.exists(), "the killed host left its socket file behind");

    std::fs::write(&script, from_agent("hi") + "\n").unwrap();
    let task = tokio::spawn(run(Config {
        socket_path: socket.clone(),
        journal_path: dir.path().join("wire.jsonl"),
        command: vec![
            helper("BOSS_AGENT_HOST_TEST_CHILD").into_os_string(),
            script.into_os_string(),
        ],
    }));
    // The child exits at once, so the socket may already be gone again; the
    // host exiting 0 is what shows the stale file did not stop it starting.
    let code = tokio::time::timeout(STEP_TIMEOUT, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(code, 0);
    assert!(!socket.exists(), "the host removes its socket on exit");
}

#[tokio::test]
async fn the_binary_exits_with_the_childs_status_and_mirrors_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("script.jsonl");
    std::fs::write(&script, from_agent("hi") + "\n").unwrap();

    let output = tokio::process::Command::new(helper("BOSS_AGENT_HOST_TEST_BIN"))
        .arg("--socket")
        .arg(dir.path().join("h.sock"))
        .arg("--journal")
        .arg(dir.path().join("wire.jsonl"))
        .arg("--")
        .arg(helper("BOSS_AGENT_HOST_TEST_CHILD"))
        .arg(&script)
        .args(["--stderr", "a line for the pane", "--exit-code", "7"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("a line for the pane"), "stderr was: {stderr}");
    let records = read_records(std::io::BufReader::new(
        std::fs::File::open(dir.path().join("wire.jsonl")).unwrap(),
    ))
    .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].line, "hi");
}
