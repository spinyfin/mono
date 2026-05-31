use std::process::ExitCode;

use repobin::run_from_env;

fn main() -> ExitCode {
    match run_from_env() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            error.exit_code()
        }
    }
}
