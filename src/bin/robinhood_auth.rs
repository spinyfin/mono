use std::env;

use broker_robinhood::{RobinhoodClient, WorkflowRoute, WorkflowScreen};

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
            print_route(&route);
        }
        Err(err) => {
            eprintln!("failed to advance workflow entry point: {err}");
            std::process::exit(1);
        }
    }

    Ok(())
}

fn print_route(route: &WorkflowRoute) {
    if let Some(replace) = &route.replace {
        println!("Route action: replace");
        print_screen(&replace.screen);
    } else {
        println!("Route action: none");
    }
}

fn print_screen(screen: &WorkflowScreen) {
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
}
