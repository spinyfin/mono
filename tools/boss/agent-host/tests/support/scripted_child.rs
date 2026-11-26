//! A line-speaking child that replays a recorded journal, for tests.
//!
//! ```text
//! scripted-child <script.jsonl> [--delay-ms N] [--stderr TEXT] [--exit-code N]
//! ```
//!
//! The script is a journal as the host writes it: one JSON object per line
//! with `dir` and `line` (`seq` and `ts_ms` are accepted and ignored, so a
//! recorded journal works unedited). The child walks the script in order:
//!
//! - a `from_agent` record is written to stdout as a line, after the optional
//!   delay;
//! - a `to_agent` record makes the child wait for a line on stdin, which must
//!   equal the record's `line`. A mismatch, or stdin closing first, exits 3
//!   after saying so on stderr.
//!
//! So a `to_agent` record is a gate: a test controls when the child moves on
//! by sending the line. When the script ends the child exits with
//! `--exit-code` (default 0). `--stderr TEXT` prints one line to stderr
//! before anything else.
//!
//! The child speaks bytes and lines only, so it can stand in for any
//! line-oriented agent; nothing here knows about ACP.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Dir {
    ToAgent,
    FromAgent,
}

#[derive(Debug, Deserialize)]
struct Step {
    dir: Dir,
    line: String,
}

struct Options {
    script: String,
    delay: Duration,
    stderr: Option<String>,
    exit_code: u8,
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let script = args.next().ok_or("usage: scripted-child <script.jsonl> [options]")?;
    let mut options = Options {
        script,
        delay: Duration::ZERO,
        stderr: None,
        exit_code: 0,
    };
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--delay-ms" => {
                options.delay = Duration::from_millis(value.parse().map_err(|_| "bad --delay-ms")?);
            }
            "--stderr" => options.stderr = Some(value),
            "--exit-code" => options.exit_code = value.parse().map_err(|_| "bad --exit-code")?,
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("scripted-child: {message}");
            return ExitCode::from(2);
        }
    };
    let script = match std::fs::read_to_string(&options.script) {
        Ok(script) => script,
        Err(err) => {
            eprintln!("scripted-child: cannot read {}: {err}", options.script);
            return ExitCode::from(2);
        }
    };
    let mut steps = Vec::new();
    for (number, text) in script.lines().enumerate().filter(|(_, text)| !text.trim().is_empty()) {
        match serde_json::from_str::<Step>(text) {
            Ok(step) => steps.push(step),
            Err(err) => {
                eprintln!("scripted-child: script line {}: {err}", number + 1);
                return ExitCode::from(2);
            }
        }
    }

    if let Some(text) = &options.stderr {
        eprintln!("{text}");
    }

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for step in steps {
        match step.dir {
            Dir::FromAgent => {
                if !options.delay.is_zero() {
                    std::thread::sleep(options.delay);
                }
                if writeln!(stdout, "{}", step.line).and_then(|()| stdout.flush()).is_err() {
                    return ExitCode::from(4);
                }
            }
            Dir::ToAgent => {
                let mut got = String::new();
                match stdin.read_line(&mut got) {
                    Ok(0) | Err(_) => {
                        eprintln!("scripted-child: stdin closed while waiting for {:?}", step.line);
                        return ExitCode::from(3);
                    }
                    Ok(_) => {}
                }
                let got = got.trim_end_matches(['\n', '\r']);
                if got != step.line {
                    eprintln!("scripted-child: expected {:?}, got {got:?}", step.line);
                    return ExitCode::from(3);
                }
            }
        }
    }
    ExitCode::from(options.exit_code)
}
