use std::env;

use broker_robinhood::RobinhoodClient;

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

    match client.initiate_login(&username, &password).await {
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
        }
        Err(err) => {
            eprintln!("failed to initiate login: {err}");
            std::process::exit(1);
        }
    }

    Ok(())
}
