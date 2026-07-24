//! engine (trunk / ci / attempts / conflicts) command handlers

use crate::*;

pub(crate) async fn run_engine_command(command: EngineCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineCommand::Status => {
            let running = engine_socket_reachable(&ctx.discovery.socket_path).await;
            let pid = running_engine_pid(&ctx.discovery.pid_file_path);
            print_entity(
                ctx,
                &serde_json::json!({
                    "running": running,
                    "pid": pid,
                    "socket_path": ctx.discovery.socket_path,
                    "pid_file_path": ctx.discovery.pid_file_path,
                }),
                || {
                    if running {
                        println!("Boss engine is running.");
                    } else {
                        println!("Boss engine is stopped.");
                    }
                    println!("Socket: {}", ctx.discovery.socket_path);
                    println!("PID file: {}", ctx.discovery.pid_file_path);
                    if let Some(pid) = pid {
                        println!("PID: {pid}");
                    }
                },
            )
        }
        EngineCommand::Start => {
            ensure_engine_running(&ctx.discovery)
                .await
                .map_err(|err| CliError::engine_unavailable(err.to_string()))?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": true, "socket_path": ctx.discovery.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Boss engine is running.");
                    }
                },
            )
        }
        EngineCommand::Stop => {
            stop_engine(&ctx.discovery.pid_file_path)
                .await
                .map_err(|err| CliError::engine_unavailable(err.to_string()))?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": false, "socket_path": ctx.discovery.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Stopped Boss engine.");
                    }
                },
            )
        }
        EngineCommand::Conflicts { command } => run_engine_conflicts_command(command, ctx).await,
        EngineCommand::Ci { command } => run_engine_ci_command(command, ctx).await,
        EngineCommand::Attempts { command } => run_engine_attempts_command(command, ctx).await,
        EngineCommand::Trunk { command } => run_engine_trunk_command(command, ctx).await,
    }
}

pub(crate) async fn run_engine_trunk_command(command: EngineTrunkCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineTrunkCommand::SetToken => run_engine_trunk_set_token(ctx).await,
        EngineTrunkCommand::Status => run_engine_trunk_status(ctx).await,
    }
}

/// Reads the Trunk API token from stdin (piped input) or an interactive,
/// non-echoing prompt. Never from argv — see `EngineTrunkCommand::SetToken`.
pub(crate) fn read_trunk_token_from_stdin_or_prompt() -> Result<String, CliError> {
    if io::stdin().is_terminal() {
        rpassword::prompt_password("Trunk API token: ").map_err(CliError::internal)
    } else {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|err| CliError::usage(format!("failed to read Trunk API token from stdin: {err}")))?;
        Ok(input.trim().to_owned())
    }
    .and_then(|token| {
        if token.is_empty() {
            Err(CliError::usage("no Trunk API token provided"))
        } else {
            Ok(token)
        }
    })
}

pub(crate) async fn run_engine_trunk_set_token(ctx: &RunContext) -> Result<(), CliError> {
    let token = read_trunk_token_from_stdin_or_prompt()?;
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::TrunkSetToken { token })
        .await
        .map_err(CliError::internal)?;
    print_trunk_status_response(ctx, response)
}

pub(crate) async fn run_engine_trunk_status(ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    let response = client
        .send_request(&FrontendRequest::TrunkStatus)
        .await
        .map_err(CliError::internal)?;
    print_trunk_status_response(ctx, response)
}

pub(crate) fn print_trunk_status_response(ctx: &RunContext, response: FrontendEvent) -> Result<(), CliError> {
    match response {
        FrontendEvent::TrunkStatus {
            configured,
            source,
            queue_check,
            note,
        } => {
            let json = serde_json::json!({
                "configured": configured,
                "source": source,
                "queue_check": queue_check,
                "note": note,
            });
            print_entity(ctx, &json, || {
                if configured {
                    let source = source.as_deref().unwrap_or("unknown");
                    println!("Trunk API token configured ({source}).");
                } else {
                    println!("No Trunk API token configured.");
                }
                if let Some(check) = &queue_check {
                    println!("Queue smoke check: {}", if check.ok { "ok" } else { "failed" });
                    println!("  {}", check.detail);
                }
                if let Some(note) = &note {
                    println!("{note}");
                }
            })
        }
        other => Err(CliError::internal(anyhow::anyhow!(
            "unexpected response to Trunk request: {other:?}"
        ))),
    }
}

pub(crate) async fn run_engine_ci_command(command: EngineCiCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineCiCommand::Classify(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::ClassifyCiRemediation {
                    attempt_id: args.attempt_id.clone(),
                    triage_class: args.class.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationClassified { attempt } => print_entity(
                    ctx,
                    &serde_json::to_value(&attempt).unwrap_or(serde_json::Value::Null),
                    || {
                        if !ctx.quiet {
                            println!(
                                "ci_remediation {} triage_class set to {}.",
                                attempt.id,
                                attempt.triage_class.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci classify", &other)),
            }
        }
        EngineCiCommand::MarkFailed(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationFailed {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationMarkedFailed { attempt } => print_entity(
                    ctx,
                    &serde_json::to_value(&attempt).unwrap_or(serde_json::Value::Null),
                    || {
                        if !ctx.quiet {
                            println!(
                                "ci_remediation {} marked failed (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-failed", &other)),
            }
        }
        EngineCiCommand::MarkRetriggered(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationRetriggered {
                    attempt_id: args.attempt_id.clone(),
                    new_id: args.new_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationRetriggered { attempt, new_id } => print_entity(
                    ctx,
                    &serde_json::json!({ "attempt": attempt, "new_id": new_id }),
                    || {
                        if !ctx.quiet {
                            println!("ci_remediation {} retrigger recorded (new id: {}).", attempt.id, new_id,);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-retriggered", &other)),
            }
        }
        EngineCiCommand::MarkSucceededViaRebase(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationSucceededViaRebase {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationSucceededViaRebase {
                    attempt,
                    budget_refunded,
                } => print_entity(
                    ctx,
                    &serde_json::json!({
                        "attempt": attempt,
                        "budget_refunded": budget_refunded,
                    }),
                    || {
                        if !ctx.quiet {
                            let refund = if budget_refunded {
                                "budget refunded"
                            } else {
                                "no budget change"
                            };
                            let sha = attempt.head_sha_after.as_deref().unwrap_or("<unknown>");
                            println!(
                                "ci_remediation {} verified green on {} — marked succeeded_via_rebase ({}).",
                                attempt.id, sha, refund,
                            );
                        }
                    },
                ),
                // A rejection is a real pass/fail receipt: the engine
                // re-probed live CI and did not find the current head
                // green, so the claim was NOT honored. Exit non-zero with
                // the live status so the worker knows to keep working
                // rather than believing a false success.
                FrontendEvent::CiRemediationSucceededViaRebaseRejected { status, live_sha, .. } => {
                    let sha = live_sha.as_deref().unwrap_or("current head");
                    Err(CliError::application(format!("CI not green on {sha}: {status}")))
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-succeeded-via-rebase", &other)),
            }
        }
        EngineCiCommand::MarkNoop(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkCiRemediationNoop {
                    attempt_id: args.attempt_id.clone(),
                    observed_sha: args.observed_sha.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationNoopValidated {
                    attempt,
                    validated_sha,
                    observed_sha,
                } => print_entity(
                    ctx,
                    &serde_json::json!({
                        "attempt": attempt,
                        "validated_sha": validated_sha,
                        "observed_sha": observed_sha,
                    }),
                    || {
                        if !ctx.quiet {
                            let sha = validated_sha.as_deref().unwrap_or("<unknown>");
                            println!(
                                "ci_remediation {} validated green on {} — attempt retired, parent unblocked.",
                                attempt.id, sha,
                            );
                            if let (Some(obs), Some(live)) = (observed_sha.as_deref(), validated_sha.as_deref())
                                && obs != live
                            {
                                println!(
                                    "(head advanced since you observed {obs}; re-validated against current head {live}.)"
                                );
                            }
                        }
                    },
                ),
                // A rejection is a real pass/fail receipt: the claim was
                // NOT honored, so exit non-zero with the live status so
                // the worker knows it must keep working.
                FrontendEvent::CiRemediationNoopRejected { status, live_sha, .. } => {
                    let sha = live_sha.as_deref().unwrap_or("current head");
                    Err(CliError::application(format!("CI not green on {sha}: {status}")))
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci mark-noop", &other)),
            }
        }
        EngineCiCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
                None => None,
            };
            // Mirror conflicts: `--limit 0` → no cap, default 50.
            let limit = match args.limit {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(50),
            };
            let response = client
                .send_request(&FrontendRequest::ListCiRemediations {
                    product_id,
                    status: args.status.clone(),
                    work_item_id: args.work_item.clone(),
                    limit,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationsList { attempts } => {
                    print_entity(ctx, &serde_json::json!({ "attempts": attempts }), || {
                        print_ci_remediations_table(&attempts)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci list", &other)),
            }
        }
        EngineCiCommand::Show(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::GetCiRemediation {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediation { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        print_ci_remediation_detail(&attempt)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci show", &other)),
            }
        }
        EngineCiCommand::Retry(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::RetryCiRemediation {
                    selector: args.selector.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationRetryDone {
                    work_item_id,
                    budget,
                    was_exhausted,
                } => print_entity(
                    ctx,
                    &serde_json::json!({
                        "work_item_id": work_item_id,
                        "budget": budget,
                        "was_exhausted": was_exhausted,
                    }),
                    || {
                        if !ctx.quiet {
                            print_ci_budget_after_retry(&work_item_id, &budget, was_exhausted);
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci retry", &other)),
            }
        }
        EngineCiCommand::Abandon(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::AbandonCiRemediation {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::CiRemediationMarkedAbandoned { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "ci_remediation {} marked abandoned (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("ci abandon", &other)),
            }
        }
        EngineCiCommand::Budget { command } => match command {
            EngineCiBudgetCommand::Show(args) => {
                let mut client = connect_for_work(ctx).await?;
                let response = client
                    .send_request(&FrontendRequest::GetCiBudget {
                        work_item_id: args.work_item_id.clone(),
                    })
                    .await
                    .map_err(CliError::internal)?;
                match response {
                    FrontendEvent::CiBudget { budget } => {
                        print_entity(ctx, &serde_json::json!({ "budget": budget }), || {
                            if !ctx.quiet {
                                print_ci_budget_snapshot(&budget);
                            }
                        })
                    }
                    FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                        Err(CliError::application(message))
                    }
                    other => Err(unexpected_event("ci budget show", &other)),
                }
            }
            EngineCiBudgetCommand::Set(args) => {
                if args.budget.is_none() && !args.clear {
                    return Err(CliError::usage(
                        "specify --budget <n> to set a per-PR override or --clear to remove it",
                    ));
                }
                let mut client = connect_for_work(ctx).await?;
                let response = client
                    .send_request(&FrontendRequest::SetCiBudget {
                        work_item_id: args.work_item_id.clone(),
                        budget: if args.clear { None } else { args.budget },
                    })
                    .await
                    .map_err(CliError::internal)?;
                match response {
                    FrontendEvent::CiBudgetUpdated { budget } => {
                        print_entity(ctx, &serde_json::json!({ "budget": budget }), || {
                            if !ctx.quiet {
                                print_ci_budget_snapshot(&budget);
                            }
                        })
                    }
                    FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                        Err(CliError::application(message))
                    }
                    other => Err(unexpected_event("ci budget set", &other)),
                }
            }
        },
    }
}

pub(crate) async fn run_engine_attempts_command(
    command: EngineAttemptsCommand,
    ctx: &RunContext,
) -> Result<(), CliError> {
    match command {
        EngineAttemptsCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
                None => None,
            };
            let limit = match args.limit {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(50),
            };
            let response = client
                .send_request(&FrontendRequest::ListEngineAttempts {
                    kinds: args.kind.clone(),
                    product_id,
                    status: args.status.clone(),
                    work_item_id: args.work_item.clone(),
                    limit,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::EngineAttemptsList { attempts } => {
                    print_entity(ctx, &serde_json::json!({ "attempts": attempts }), || {
                        print_engine_attempts_table(&attempts)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("attempts list", &other)),
            }
        }
    }
}

pub(crate) async fn run_engine_conflicts_command(
    command: EngineConflictsCommand,
    ctx: &RunContext,
) -> Result<(), CliError> {
    match command {
        EngineConflictsCommand::List(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product_id = match args.product.clone() {
                Some(selector) => Some(resolve_product(&mut client, Some(selector), ctx).await?.id),
                None => None,
            };
            // CLI-side default cap so human output stays readable; an
            // explicit `--limit 0` is treated as "no cap" so JSON
            // callers can stream everything.
            let limit = match args.limit {
                Some(0) => None,
                Some(n) => Some(n),
                None => Some(50),
            };
            let response = client
                .send_request(&FrontendRequest::ListConflictResolutions {
                    product_id,
                    status: args.status.clone(),
                    work_item_id: args.work_item.clone(),
                    limit,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionsList { attempts } => {
                    print_entity(ctx, &serde_json::json!({ "attempts": attempts }), || {
                        print_conflict_resolutions_table(&attempts)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts list", &other)),
            }
        }
        EngineConflictsCommand::Show(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::GetConflictResolution {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolution { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        print_conflict_resolution_detail(&attempt)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts show", &other)),
            }
        }
        EngineConflictsCommand::Retry(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::RetryConflictResolution {
                    attempt_id: args.attempt_id.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionRetried { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} reset to pending; engine will re-dispatch a worker.",
                                attempt.id,
                            );
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts retry", &other)),
            }
        }
        EngineConflictsCommand::Abandon(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::AbandonConflictResolution {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionMarkedAbandoned { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} marked abandoned (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts abandon", &other)),
            }
        }
        EngineConflictsCommand::MarkFailed(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::MarkConflictResolutionFailed {
                    attempt_id: args.attempt_id.clone(),
                    reason: args.reason.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolutionMarkedFailed { attempt } => print_entity(
                    ctx,
                    &serde_json::to_value(&attempt).unwrap_or(serde_json::Value::Null),
                    || {
                        if !ctx.quiet {
                            println!(
                                "Conflict resolution {} marked failed (reason: {}).",
                                attempt.id,
                                attempt.failure_reason.as_deref().unwrap_or("<unset>"),
                            );
                        }
                    },
                ),
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts mark-failed", &other)),
            }
        }
        EngineConflictsCommand::RecordProducer(args) => {
            let mut client = connect_for_work(ctx).await?;
            let response = client
                .send_request(&FrontendRequest::RecordProducerSideConflict {
                    execution_id: args.execution_id.clone(),
                    head_branch: args.head_branch.clone(),
                    base_branch: args.base_branch.clone(),
                    conflicted_files: args.files.clone(),
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictResolution { attempt } => {
                    print_entity(ctx, &serde_json::json!({ "attempt": attempt }), || {
                        if !ctx.quiet {
                            println!(
                                "Producer-side conflict recorded ({}) for telemetry — {} file(s), class={}.",
                                attempt.id,
                                args.files.len(),
                                attempt.conflict_class.as_deref().unwrap_or("unknown"),
                            );
                        }
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts record-producer", &other)),
            }
        }
        EngineConflictsCommand::Hotspots(args) => {
            let mut client = connect_for_work(ctx).await?;
            let product = resolve_product(&mut client, args.product.clone(), ctx).await?;
            let response = client
                .send_request(&FrontendRequest::GetConflictHotspots {
                    product_id: product.id.clone(),
                    top: args.top,
                })
                .await
                .map_err(CliError::internal)?;
            match response {
                FrontendEvent::ConflictHotspots { report } => {
                    print_entity(ctx, &serde_json::json!({ "report": report }), || {
                        print_conflict_hotspot_report(&report)
                    })
                }
                FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
                    Err(CliError::application(message))
                }
                other => Err(unexpected_event("conflicts hotspots", &other)),
            }
        }
    }
}

pub(crate) fn print_conflict_hotspot_report(report: &ConflictHotspotReport) {
    println!(
        "Conflict hotspots for product {} ({} event(s) scanned)",
        report.product_id, report.total_events,
    );

    println!("\nBy class:");
    let mut class_table = new_dynamic_table(vec!["CLASS", "COUNT"]);
    for entry in &report.class_counts {
        class_table.add_row(vec![entry.class.as_str(), entry.count.to_string().as_str()]);
    }
    print_table(class_table);

    println!("\nTop conflicted files:");
    let mut file_table = new_dynamic_table(vec!["FILE", "COUNT"]);
    for entry in &report.file_frequency {
        file_table.add_row(vec![entry.path.as_str(), entry.count.to_string().as_str()]);
    }
    print_table(file_table);

    println!("\nTop co-conflicting file pairs:");
    let mut pair_table = new_dynamic_table(vec!["FILE A", "FILE B", "COUNT"]);
    for entry in &report.file_pair_frequency {
        pair_table.add_row(vec![
            entry.path_a.as_str(),
            entry.path_b.as_str(),
            entry.count.to_string().as_str(),
        ]);
    }
    print_table(pair_table);
}

/// The trailing cells shared by the attempt list tables: pr_url, work_item_id
/// (falling back to `""` when absent), failure_reason (likewise), created_at.
/// Callers supply their own differing leading columns.
pub(crate) fn attempt_trailing_cells<'a>(
    pr_url: &'a str,
    work_item_id: Option<&'a str>,
    failure_reason: Option<&'a str>,
    created_at: &'a str,
) -> [&'a str; 4] {
    [
        pr_url,
        work_item_id.unwrap_or(""),
        failure_reason.unwrap_or(""),
        created_at,
    ]
}

pub(crate) fn print_conflict_resolutions_table(attempts: &[ConflictResolution]) {
    let mut table = new_dynamic_table(vec!["ID", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED"]);
    for attempt in attempts {
        let mut cells = vec![attempt.id.as_str(), attempt.status.as_str()];
        cells.extend(attempt_trailing_cells(
            attempt.pr_url.as_str(),
            Some(attempt.work_item_id.as_str()),
            attempt.failure_reason.as_deref(),
            attempt.created_at.as_str(),
        ));
        table.add_row(cells);
    }
    print_table(table);
}

pub(crate) fn print_conflict_resolution_detail(attempt: &ConflictResolution) {
    DetailTable::new()
        .row("id", &attempt.id)
        .row("status", &attempt.status)
        .row("product_id", &attempt.product_id)
        .row("work_item_id", &attempt.work_item_id)
        .row("pr_url", &attempt.pr_url)
        .row("pr_number", attempt.pr_number.to_string())
        .row("head_branch", &attempt.head_branch)
        .row("base_branch", &attempt.base_branch)
        .opt_row("base_sha_at_trigger", attempt.base_sha_at_trigger.clone())
        .opt_row("head_sha_before", attempt.head_sha_before.clone())
        .opt_row("head_sha_after", attempt.head_sha_after.clone())
        .opt_row("failure_reason", attempt.failure_reason.clone())
        .lifecycle_rows(
            attempt.cube_lease_id.clone(),
            attempt.cube_workspace_id.clone(),
            attempt.worker_id.clone(),
            &attempt.created_at,
            attempt.started_at.clone(),
            attempt.finished_at.clone(),
        )
        .print();
    if let Some(diag) = &attempt.conflict_diagnosis {
        println!();
        println!("conflict_diagnosis (raw):");
        println!("{diag}");
    }
}

pub(crate) fn print_ci_remediations_table(attempts: &[CiRemediation]) {
    let mut table = new_dynamic_table(vec!["ID", "KIND", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED"]);
    for attempt in attempts {
        let mut cells = vec![
            attempt.id.as_str(),
            attempt.attempt_kind.as_str(),
            attempt.status.as_str(),
        ];
        cells.extend(attempt_trailing_cells(
            attempt.pr_url.as_str(),
            Some(attempt.work_item_id.as_str()),
            attempt.failure_reason.as_deref(),
            attempt.created_at.as_str(),
        ));
        table.add_row(cells);
    }
    print_table(table);
}

pub(crate) fn print_ci_remediation_detail(attempt: &CiRemediation) {
    DetailTable::new()
        .row("id", &attempt.id)
        .row("status", &attempt.status)
        .row("attempt_kind", &attempt.attempt_kind)
        .row("consumes_budget", attempt.consumes_budget.to_string())
        .row("product_id", &attempt.product_id)
        .row("work_item_id", &attempt.work_item_id)
        .row("pr_url", &attempt.pr_url)
        .row("pr_number", attempt.pr_number.to_string())
        .row("head_branch", &attempt.head_branch)
        .row("head_sha_at_trigger", &attempt.head_sha_at_trigger)
        .opt_row("head_sha_after", attempt.head_sha_after.clone())
        .opt_row("triage_class", attempt.triage_class.clone())
        .opt_row("failure_reason", attempt.failure_reason.clone())
        .lifecycle_rows(
            attempt.cube_lease_id.clone(),
            attempt.cube_workspace_id.clone(),
            attempt.worker_id.clone(),
            &attempt.created_at,
            attempt.started_at.clone(),
            attempt.finished_at.clone(),
        )
        .print();
    if !attempt.failed_checks.is_empty() {
        println!();
        println!("failed_checks (raw):");
        println!("{}", attempt.failed_checks);
    }
    if let Some(log) = &attempt.log_excerpt {
        println!();
        println!("log_excerpt:");
        println!("{log}");
    }
}

pub(crate) fn print_ci_budget_snapshot(snapshot: &CiBudgetSnapshot) {
    let override_text = match snapshot.per_pr_override {
        Some(n) => n.to_string(),
        None => "<inherit>".to_owned(),
    };
    let blocked = snapshot.blocked_reason.clone().unwrap_or_else(|| "—".to_owned());
    DetailTable::new()
        .row("work_item_id", &snapshot.work_item_id)
        .row("per_pr_override", override_text)
        .row("product_default", snapshot.product_default.to_string())
        .row("effective", snapshot.effective.to_string())
        .row("used", snapshot.used.to_string())
        .row("blocked_reason", blocked)
        .print();
}

pub(crate) fn print_ci_budget_after_retry(work_item_id: &str, budget: &CiBudgetSnapshot, was_exhausted: bool) {
    if was_exhausted {
        println!(
            "Reset ci_attempts_used for {} (used: {}/{} effective).",
            work_item_id, budget.used, budget.effective,
        );
        println!("Cleared blocked_reason='ci_failure_exhausted'.");
        println!("Parent will re-enter in_review on next probe; engine will auto-fix on detection of failure.",);
    } else {
        println!(
            "Reset ci_attempts_used for {} (used: {}/{} effective).",
            work_item_id, budget.used, budget.effective,
        );
        println!("Parent was not exhausted; no status change.");
    }
}

pub(crate) fn print_engine_attempts_table(attempts: &[EngineAttemptListEntry]) {
    let mut table = new_dynamic_table(vec!["KIND", "ID", "STATUS", "PR", "WORK ITEM", "REASON", "CREATED"]);
    for row in attempts {
        let mut cells = vec![row.kind.as_str(), row.id.as_str(), row.status.as_str()];
        cells.extend(attempt_trailing_cells(
            row.pr_url.as_str(),
            row.work_item_id.as_deref(),
            row.failure_reason.as_deref(),
            row.created_at.as_str(),
        ));
        table.add_row(cells);
    }
    print_table(table);
}

/// Human-readable rendering for `boss product audit-effort`. The
/// JSON shape (under `--json`) is the `EffortAuditReport` directly;
/// this is the table the report-shape example in design §Q4
/// follow-up shows.
pub(crate) fn print_effort_audit_report(report: &EffortAuditReport) {
    let window = match report.window_days {
        Some(n) => format!("last {n} days"),
        None => "all recorded escalations".to_owned(),
    };
    println!(
        "Marker analysis ({window}, {n_esc} escalations across {n_chores} chores):",
        n_esc = report.total_escalations,
        n_chores = report.total_chores,
    );
    if report.rows.is_empty() {
        println!();
        println!(
            "  No marker matches recorded yet. Either no chores have been filed against this \
             product or no escalation events are recorded.",
        );
        return;
    }
    let mut table = new_dynamic_table(vec![
        "MARKER",
        "ORIG LEVEL",
        "MATCHES",
        "ESCALATIONS",
        "UNDER-CLASS RATE",
        "NOTE",
    ]);
    for row in &report.rows {
        let rate = match row.under_class_rate {
            Some(r) => format!("{:.1}%", r * 100.0),
            None => "—".to_owned(),
        };
        let annotation = row.annotation.clone().unwrap_or_default();
        table.add_row(vec![
            row.marker.as_str(),
            row.original_level.as_str(),
            &row.matches.to_string(),
            &row.escalations.to_string(),
            rate.as_str(),
            annotation.as_str(),
        ]);
    }
    print_table(table);
    println!();
    println!(
        "Threshold for the \"consider promoting\" callout: under-class rate > {:.0}%. \
         Edit the §Q4 marker lists in code based on this report; v1 keeps the \
         heuristic code-defined.",
        report.under_class_threshold * 100.0,
    );
}
