use std::process::ExitCode;

use clap::Parser;
use cube::{cli::Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;

    match run(cli) {
        Ok(result) => {
            if json {
                match serde_json::to_string_pretty(&result) {
                    Ok(output) => println!("{output}"),
                    Err(error) => {
                        eprintln!("error: failed to encode JSON output: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                println!("{}", result.message);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if json {
                let payload = serde_json::json!({
                    "error": error.to_string(),
                });
                match serde_json::to_string_pretty(&payload) {
                    Ok(output) => eprintln!("{output}"),
                    Err(encoding_error) => {
                        eprintln!("error: {error}");
                        eprintln!("error: failed to encode JSON output: {encoding_error}");
                    }
                }
            } else {
                eprintln!("error: {error}");
            }
            error.exit_code()
        }
    }
}
