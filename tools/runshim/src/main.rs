use std::process::ExitCode;

use runshim::run_from_env;

fn main() -> ExitCode {
    match run_from_env() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            error.exit_code()
        }
    }
}
