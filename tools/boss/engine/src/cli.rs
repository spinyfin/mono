use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, Eq, PartialEq, ValueEnum)]
pub enum Mode {
    Cli,
    Server,
}

#[derive(Debug, Parser)]
#[command(name = "boss-engine")]
pub struct Cli {
    #[arg(long, value_enum, default_value_t = Mode::Cli)]
    pub mode: Mode,

    #[arg(long)]
    pub socket_path: Option<String>,

    #[arg(long)]
    pub prompt: Option<String>,
}
