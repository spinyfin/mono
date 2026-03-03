use std::{env, time::Duration};

use broker_robinhood::{RobinhoodClient, WorkflowRoute, WorkflowScreen};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);

    let username = match args.next() {
        Some(value) => value,
        None => {
            eprintln!("usage: robinhood-auth <username> <password>");
            std::process::exit(64);
        }
    };

    let password = match args.next() {
        Some(value) => value,
        None => {
            eprintln!("usage: robinhood-auth <username> <password>");
            std::process::exit(64);
        }
    };

    if args.next().is_some() {
        eprintln!("usage: robinhood-auth <username> <password>");
        std::process::exit(64);
    }

    let client = RobinhoodClient::new("https://api.robinhood.com")?;

    let challenge = match client.initiate_login(&username, &password).await {
        Ok(challenge) => {
            println!(
                "Verification workflow ID: {}",
                challenge.verification_workflow().id
            );
            println!(
                "Workflow status: {}",
                challenge.verification_workflow().workflow_status
            );
            println!("Device token: {}", challenge.device_token());
            println!("Request ID: {}", challenge.request_id());
            challenge
        }
        Err(err) => {
            eprintln!("failed to initiate login: {err}");
            std::process::exit(1);
        }
    };

    let device_token = challenge.device_token();
    let request_id = challenge.request_id();

    match client
        .fetch_verification_result(&challenge.verification_workflow().id)
        .await
    {
        Ok(result) => {
            println!("Verification result: {result}");
        }
        Err(err) => {
            eprintln!("failed to fetch verification result: {err}");
            std::process::exit(1);
        }
    }

    match client
        .advance_workflow_entry_point(&challenge.verification_workflow().id)
        .await
    {
        Ok(route) => {
            let challenge_id = print_route(&route);
            let status = wait_for_push_validation(&client, challenge_id.as_deref()).await?;

            match status.as_deref() {
                Some("validated") => {
                    complete_device_approval(&client, &challenge.verification_workflow().id)
                        .await?;

                    match client
                        .finalize_login(&username, &password, &device_token, &request_id)
                        .await
                    {
                        Ok(token) => {
                            let formatted = serde_json::to_string_pretty(&token)?;
                            println!("OAuth token response:\n{formatted}");
                        }
                        Err(err) => {
                            eprintln!("failed to fetch final oauth token: {err}");
                            std::process::exit(1);
                        }
                    }
                }
                Some("expired") => {
                    println!("Push challenge expired; rerun the CLI to restart auth.");
                }
                Some(other) => {
                    println!(
                        "Push challenge ended with status `{other}`; skipping approval completion."
                    );
                }
                None => {}
            }
        }
        Err(err) => {
            eprintln!("failed to advance workflow entry point: {err}");
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_route(route: &WorkflowRoute) -> Option<String> {
    if let Some(replace) = &route.replace {
        println!("Route action: replace");
        print_screen(&replace.screen)
    } else {
        println!("Route action: none");
        None
    }
}

fn print_screen(screen: &WorkflowScreen) -> Option<String> {
    let mut challenge_id = None;

    println!("Screen name: {}", screen.name);
    if let Some(block_id) = &screen.block_id {
        println!("Screen block ID: {block_id}");
    }

    if let Some(params) = &screen.device_approval_challenge_screen_params {
        println!(
            "Device approval challenge flow ID: {}",
            params.sheriff_flow_id.as_deref().unwrap_or("<unknown>")
        );

        if let Some(challenge) = &params.sheriff_challenge {
            if let Some(id) = &challenge.id {
                println!("Challenge ID: {id}");
                challenge_id = Some(id.clone());
            }
            if let Some(challenge_type) = &challenge.challenge_type {
                println!("Challenge type: {challenge_type}");
            }
            if let Some(status) = &challenge.status {
                println!("Challenge status: {status}");
            }
            if let Some(retries) = challenge.remaining_retries {
                println!("Remaining retries: {retries}");
            }
            if let Some(attempts) = challenge.remaining_attempts {
                println!("Remaining attempts: {attempts}");
            }
            if let Some(expires_at) = &challenge.expires_at {
                println!("Challenge expires at: {expires_at}");
            }
        }

        if let Some(fallback) = &params.fallback_cta_text {
            println!("Fallback CTA text: {fallback}");
        }
    }

    challenge_id
}

async fn wait_for_push_validation(
    client: &RobinhoodClient,
    challenge_id: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Some(challenge_id) = challenge_id else {
        println!("No sheriff challenge to poll.");
        return Ok(None);
    };

    const MAX_ATTEMPTS: usize = 30;
    const DELAY: Duration = Duration::from_secs(2);

    let mut last_status: Option<String> = None;

    for attempt in 1..=MAX_ATTEMPTS {
        let status = match client.fetch_push_prompt_status(challenge_id).await {
            Ok(status) => status,
            Err(err) => {
                eprintln!("failed to fetch push prompt status: {err}");
                std::process::exit(1);
            }
        };

        println!("Push challenge status (attempt {attempt}/{MAX_ATTEMPTS}): {status}");

        match status.as_str() {
            "validated" | "expired" => return Ok(Some(status)),
            _ => {
                last_status = Some(status);
                sleep(DELAY).await;
            }
        }
    }

    println!(
        "Push challenge did not validate within allotted attempts (last status: {}).",
        last_status.as_deref().unwrap_or("<unknown>")
    );

    Ok(last_status)
}

async fn complete_device_approval(
    client: &RobinhoodClient,
    workflow_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match client.complete_device_approval(workflow_id).await {
        Ok(route) => {
            if let Some(exit) = route.exit {
                println!("Workflow exit status: {}", exit.status);
            } else {
                println!("Workflow completed without exit status.");
            }
            Ok(())
        }
        Err(err) => {
            eprintln!("failed to complete device approval: {err}");
            std::process::exit(1);
        }
    }
}
