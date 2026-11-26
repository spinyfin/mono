//! `boss-acp-probe sdk-attach`: the official SDK's connection type on a
//! stream that an earlier client already initialised, against a real agent.
//!
//! Phase one plays the earlier client process. It writes `initialize`,
//! `session/new` and one `session/prompt` as plain lines with its own request
//! ids, and stops as soon as the prompt is on the wire: that prompt's
//! response is still outstanding, as it would be for an engine that restarted
//! mid-turn. Phase two hands the same stream to the SDK's v1 client
//! connection, which sends a second prompt.
//!
//! The report says what the SDK connection put on the wire first, which id it
//! reported for its own request, and whether anything it exposes saw the
//! response to the earlier client's prompt. Every line goes to the journal,
//! so the report can be checked against what was actually on the wire.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::{Agent, Client, Handled, Lines, v1};
use anyhow::{Context, Result, anyhow, bail};
use boss_acp_client::JsonRpcMessage;
use boss_acp_client::journal::{Direction, Journal, WireRecord, read_records};
use boss_acp_client::schema::{
    ContentBlock, PromptRequest, SessionConfigId, SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent,
};
use clap::Args;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::events::emit;
use crate::{
    SessionArgs, SpawnedAgent, config_assignments, initialize_request, new_session_request, prepare_run, spawn_agent,
};

#[derive(Args)]
pub struct SdkAttachArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// The prompt the earlier client sends and never sees answered.
    #[arg(long, default_value = "Reply with exactly the word: first")]
    earlier_prompt: String,
    /// The prompt the SDK connection sends after it attaches.
    #[arg(long, default_value = "Reply with exactly the word: second")]
    attached_prompt: String,
    /// How long the SDK connection may take to get its own response.
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
}

type FromAgent = UnboundedReceiver<std::io::Result<String>>;

pub async fn run(args: SdkAttachArgs) -> Result<()> {
    // Everything that can be refused is refused before a file is created or a
    // process is started.
    let environment = prepare_run(&args.session)?;
    let journal_path = args.session.journal.clone();
    let journal = Arc::new(Journal::create(&journal_path).with_context(|| format!("creating {journal_path:?}"))?);
    let SpawnedAgent {
        mut child,
        mut stdin,
        stdout,
        environment,
        ..
    } = spawn_agent(&args.session, environment)?;

    // One writer and one reader own the child's pipes for the whole run, so
    // both phases share a single journal in wire order.
    let (to_agent, mut outgoing) = unbounded::<String>();
    let (incoming, mut from_agent) = unbounded::<std::io::Result<String>>();
    let writer_journal = journal.clone();
    tokio::spawn(async move {
        while let Some(line) = outgoing.next().await {
            let framed = format!("{line}\n");
            if writer_journal.append(Direction::ToAgent, &line).is_err()
                || stdin.write_all(framed.as_bytes()).await.is_err()
                || stdin.flush().await.is_err()
            {
                break;
            }
        }
        // Dropping the pipe here is the agent's end of input.
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let delivered = match lines.next_line().await {
                Ok(Some(line)) if line.trim().is_empty() => continue,
                Ok(Some(line)) => journal.append(Direction::FromAgent, &line).map(|_| line),
                Ok(None) => break,
                Err(error) => Err(error),
            };
            let failed = delivered.is_err();
            if incoming.unbounded_send(delivered).is_err() || failed {
                break;
            }
        }
    });

    // ── Phase one: the earlier client ────────────────────────────────────────
    let mut next_id = 0_i64;
    let mut earlier_call = async |request: (String, Value), from_agent: &mut FromAgent| -> Result<(i64, Value)> {
        next_id += 1;
        send_line(&to_agent, next_id, &request)?;
        Ok((next_id, await_response(from_agent, next_id, &request.0).await?))
    };
    let (initialize_id, _) = earlier_call(untyped(&initialize_request(&args.session))?, &mut from_agent).await?;
    let (session_new_id, session) =
        earlier_call(untyped(&new_session_request(&args.session)?)?, &mut from_agent).await?;
    let session_id = session
        .get("sessionId")
        .and_then(Value::as_str)
        .context("session/new returned no sessionId")?
        .to_owned();
    for (option, value) in config_assignments(&args.session)? {
        let request = SetSessionConfigOptionRequest::new(session_id.clone(), SessionConfigId::new(option), value);
        earlier_call(untyped(&request)?, &mut from_agent).await?;
    }
    if let Some(mode) = &args.session.mode {
        let request = SetSessionModeRequest::new(session_id.clone(), mode.clone());
        earlier_call(untyped(&request)?, &mut from_agent).await?;
    }
    let prompt = |text: &str| PromptRequest::new(session_id.clone(), vec![ContentBlock::Text(TextContent::new(text))]);
    let unanswered_id = next_id + 1;
    send_line(&to_agent, unanswered_id, &untyped(&prompt(&args.earlier_prompt))?)?;
    emit(json!({
        "event": "earlier_client",
        "session_id": session_id,
        "initialize_request_id": initialize_id,
        "session_new_request_id": session_new_id,
        "unanswered_prompt_request_id": unanswered_id,
        "environment": environment.report,
    }));

    // ── Phase two: the SDK's connection on the same stream ───────────────────
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorder = seen.clone();
    let transport = Lines::new(to_agent.clone().sink_map_err(std::io::Error::other), from_agent);
    let attached_prompt = prompt(&args.attached_prompt);
    let conversation = Client
        .v1()
        .name("boss-acp-probe-sdk-attach")
        .on_receive_dispatch(
            async move |dispatch: v1::Dispatch, _connection: v1::ConnectionTo<Agent>| {
                let description = match &dispatch {
                    v1::Dispatch::Request(request, _) => format!("request {}", request.method()),
                    v1::Dispatch::Notification(notification) => format!("notification {}", notification.method()),
                    v1::Dispatch::Response(_, router) => {
                        format!("response to {} id {}", router.method(), json!(router.id()))
                    }
                };
                recorder
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(description);
                // Seen, not claimed: a response still has to reach the future
                // that waits for it.
                Ok(Handled::No {
                    message: dispatch,
                    retry: false,
                })
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(transport, async |connection: v1::ConnectionTo<Agent>| {
            let sent = connection.send_request(attached_prompt);
            let reported_id = json!(sent.id());
            emit(json!({"event": "sdk_request_sent", "method": "session/prompt", "reported_request_id": reported_id}));
            let response = sent.block_task().await?;
            Ok((reported_id, json!(response.stop_reason)))
        });
    let outcome = tokio::time::timeout(Duration::from_secs(args.timeout_secs), conversation).await;

    // The agent's end of input: every sender of outgoing lines is gone.
    drop(to_agent);
    let exit = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    if exit.is_err() {
        let _ = child.start_kill();
    }

    let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let (reported_id, stop_reason, error) = match outcome {
        Ok(Ok((id, stop_reason))) => (id, stop_reason, Value::Null),
        Ok(Err(error)) => (Value::Null, Value::Null, json!(error.to_string())),
        Err(_) => (
            Value::Null,
            Value::Null,
            json!("timed out waiting for the SDK connection's own response"),
        ),
    };
    emit(report(
        &journal_path,
        unanswered_id,
        &reported_id,
        &stop_reason,
        &error,
        &seen,
    )?);
    Ok(())
}

/// The method and params of a typed message.
fn untyped<M: JsonRpcMessage>(message: &M) -> Result<(String, Value)> {
    message
        .to_untyped_message()
        .map(|untyped| untyped.into_parts())
        .map_err(|error| anyhow!("encoding {}: {error}", message.method()))
}

fn send_line(to_agent: &UnboundedSender<String>, id: i64, (method, params): &(String, Value)) -> Result<()> {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    to_agent
        .unbounded_send(line)
        .map_err(|_| anyhow!("the agent's input closed before {method} could be sent"))
}

/// Reads until the response to request `id` arrives. Everything else the
/// agent sends meanwhile is journalled and passed over.
async fn await_response(from_agent: &mut FromAgent, id: i64, method: &str) -> Result<Value> {
    loop {
        let line = from_agent
            .next()
            .await
            .with_context(|| format!("the agent closed the stream before answering {method}"))?
            .with_context(|| format!("reading the response to {method}"))?;
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("method").is_some() || message.get("id") != Some(&json!(id)) {
            continue;
        }
        if let Some(error) = message.get("error") {
            bail!("{method} failed: {error}");
        }
        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// What the journal and the SDK's dispatch handler show about phase two.
fn report(
    journal: &Path,
    unanswered_id: i64,
    reported_id: &Value,
    stop_reason: &Value,
    error: &Value,
    seen: &[String],
) -> Result<Value> {
    let file = std::fs::File::open(journal).with_context(|| format!("reading {journal:?}"))?;
    let records = read_records(std::io::BufReader::new(file))?;
    Ok(summarize(
        &records,
        unanswered_id,
        reported_id,
        stop_reason,
        error,
        seen,
    ))
}

fn summarize(
    records: &[WireRecord],
    unanswered_id: i64,
    reported_id: &Value,
    stop_reason: &Value,
    error: &Value,
    seen: &[String],
) -> Value {
    let parsed: Vec<(&WireRecord, Value)> = records
        .iter()
        .filter_map(|record| serde_json::from_str(&record.line).ok().map(|message| (record, message)))
        .collect();
    // The earlier client's last line is its unanswered prompt.
    let handover = parsed
        .iter()
        .find(|(record, message)| record.dir == Direction::ToAgent && message.get("id") == Some(&json!(unanswered_id)))
        .map(|(record, _)| record.seq);
    let after_handover = |record: &WireRecord| handover.is_some_and(|seq| record.seq > seq);
    let from_sdk: Vec<&(&WireRecord, Value)> = parsed
        .iter()
        .filter(|(record, _)| record.dir == Direction::ToAgent && after_handover(record))
        .collect();
    let is_response = |message: &Value| message.get("method").is_none() && message.get("id").is_some();
    let old_response = parsed.iter().find(|(record, message)| {
        record.dir == Direction::FromAgent && is_response(message) && message.get("id") == Some(&json!(unanswered_id))
    });
    let responses_on_wire = parsed
        .iter()
        .filter(|(record, message)| {
            record.dir == Direction::FromAgent && after_handover(record) && is_response(message)
        })
        .count();
    let responses_seen = seen.iter().filter(|entry| entry.starts_with("response to ")).count();
    let old_response_seen = seen
        .iter()
        .any(|entry| entry.ends_with(&format!(" id {unanswered_id}")));
    let mut seen_counts: Vec<(String, usize)> = Vec::new();
    for entry in seen {
        match seen_counts.iter_mut().find(|(existing, _)| existing == entry) {
            Some((_, count)) => *count += 1,
            None => seen_counts.push((entry.clone(), 1)),
        }
    }
    json!({
        "event": "sdk_attach_report",
        "earlier_prompt_request_id": unanswered_id,
        "handover_after_seq": handover,
        "first_line_from_sdk": from_sdk.first().map(|(record, message)| json!({
            "seq": record.seq,
            "method": message.get("method"),
            "id": message.get("id"),
        })),
        "initialize_sent_by_sdk": from_sdk.iter().any(|(_, message)| message.get("method") == Some(&json!("initialize"))),
        "sdk_reported_request_id": reported_id,
        "sdk_own_stop_reason": stop_reason,
        "sdk_error": error,
        "earlier_response_on_wire_seq": old_response.map(|(record, _)| record.seq),
        "earlier_response_stop_reason": old_response.and_then(|(_, message)| message.pointer("/result/stopReason").cloned()),
        "earlier_response_seen_by_sdk_handler": old_response_seen,
        "responses_on_wire_after_handover": responses_on_wire,
        "responses_seen_by_sdk_handler": responses_seen,
        "sdk_handler_saw": seen_counts.into_iter().map(|(entry, count)| json!({"dispatch": entry, "count": count})).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u64, dir: Direction, line: Value) -> WireRecord {
        WireRecord {
            seq,
            ts_ms: 0,
            dir,
            line: line.to_string(),
        }
    }

    #[test]
    fn the_report_separates_what_was_on_the_wire_from_what_the_sdk_handler_saw() {
        let records = [
            record(
                1,
                Direction::ToAgent,
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
            ),
            record(
                2,
                Direction::FromAgent,
                json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
            ),
            record(
                3,
                Direction::ToAgent,
                json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt"}),
            ),
            record(
                4,
                Direction::ToAgent,
                json!({"jsonrpc": "2.0", "id": "uuid-a", "method": "session/prompt"}),
            ),
            record(
                5,
                Direction::FromAgent,
                json!({"jsonrpc": "2.0", "method": "session/update"}),
            ),
            record(
                6,
                Direction::FromAgent,
                json!({"jsonrpc": "2.0", "id": 2, "result": {"stopReason": "end_turn"}}),
            ),
            record(
                7,
                Direction::FromAgent,
                json!({"jsonrpc": "2.0", "id": "uuid-a", "result": {"stopReason": "end_turn"}}),
            ),
        ];
        let seen = [
            "notification session/update".to_owned(),
            "response to session/prompt id \"uuid-a\"".to_owned(),
        ];
        let report = summarize(&records, 2, &json!("uuid-a"), &json!("end_turn"), &Value::Null, &seen);

        assert_eq!(report["handover_after_seq"], 3);
        assert_eq!(
            report["first_line_from_sdk"],
            json!({"seq": 4, "method": "session/prompt", "id": "uuid-a"})
        );
        assert_eq!(report["initialize_sent_by_sdk"], false);
        assert_eq!(report["earlier_response_on_wire_seq"], 6);
        assert_eq!(report["earlier_response_stop_reason"], "end_turn");
        assert_eq!(report["earlier_response_seen_by_sdk_handler"], false);
        assert_eq!(report["responses_on_wire_after_handover"], 2);
        assert_eq!(report["responses_seen_by_sdk_handler"], 1);
    }

    #[tokio::test]
    async fn a_refused_run_leaves_every_file_it_was_pointed_at_untouched() {
        let (scratch, production) = crate::tests::scratch_with_production_directory("sdk-attach");
        let journal = crate::tests::sentinel(&production, "journal.jsonl");
        let stderr = crate::tests::sentinel(&production, "agent.log");
        let elsewhere_journal = scratch.join("journal.jsonl");
        let elsewhere_stderr = scratch.join("agent.log");
        let scratch_text = scratch.to_string_lossy().into_owned();
        for (journal_flag, stderr_flag) in [
            (&journal, &elsewhere_stderr),
            (&elsewhere_journal, &stderr),
            (&journal, &stderr),
        ] {
            let argv = [
                "boss-acp-probe".to_owned(),
                "sdk-attach".to_owned(),
                "--cwd".to_owned(),
                scratch_text.clone(),
                "--journal".to_owned(),
                journal_flag.to_string_lossy().into_owned(),
                "--agent-stderr".to_owned(),
                stderr_flag.to_string_lossy().into_owned(),
                "--".to_owned(),
                "/nonexistent/agent".to_owned(),
            ];
            let crate::Mode::SdkAttach(args) = <crate::Cli as clap::Parser>::try_parse_from(argv).unwrap().command
            else {
                panic!("parsed the wrong subcommand");
            };
            let error = run(*args).await.unwrap_err().to_string();
            assert!(error.contains("production Boss path"), "{error}");
        }
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), "sentinel");
        assert_eq!(std::fs::read_to_string(&stderr).unwrap(), "sentinel");
        assert!(!elsewhere_journal.exists(), "a refused run created no journal");
        assert!(!elsewhere_stderr.exists(), "a refused run created no stderr file");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn a_handler_that_did_see_the_earlier_response_is_reported_as_such() {
        let records = [
            record(
                1,
                Direction::ToAgent,
                json!({"jsonrpc": "2.0", "id": 5, "method": "session/prompt"}),
            ),
            record(
                2,
                Direction::FromAgent,
                json!({"jsonrpc": "2.0", "id": 5, "result": {}}),
            ),
        ];
        let seen = ["response to session/prompt id 5".to_owned()];
        let report = summarize(&records, 5, &Value::Null, &Value::Null, &Value::Null, &seen);
        assert_eq!(report["earlier_response_seen_by_sdk_handler"], true);
        assert_eq!(report["first_line_from_sdk"], Value::Null);
    }
}
