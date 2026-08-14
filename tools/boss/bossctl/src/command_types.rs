//! Shared value enums used by the `bossctl` command surface.

/// Output format for `bossctl agents transcript --format`.
#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
pub(crate) enum TranscriptFormat {
    /// Plain-text summary (default).
    Text,
    /// Raw JSONL lines as emitted by Claude Code.
    Jsonl,
    /// Converted markdown via the engine's transcript renderer.
    Markdown,
}

impl std::fmt::Display for TranscriptFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text => write!(f, "text"),
            Self::Jsonl => write!(f, "jsonl"),
            Self::Markdown => write!(f, "markdown"),
        }
    }
}

/// Which engine log or diagnostic stream `bossctl logs` should read.
#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
pub(crate) enum LogSource {
    /// `engine-trace.jsonl` — structured tracing events (primary log).
    Engine,
    /// `engine-audit.log` — lifecycle events (start, socket bind, shutdown).
    Audit,
    /// `dispatch-events/current.jsonl` — dispatch pipeline stage events.
    Dispatch,
    /// `diagnostics/spawn-YYYY-MM-DD.jsonl` — worker-spawn diagnostics.
    Spawn,
    /// App + engine population-timing day files under `diagnostics/`.
    #[value(name = "population-timing")]
    PopulationTiming,
}

impl std::fmt::Display for LogSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine => write!(f, "engine"),
            Self::Audit => write!(f, "audit"),
            Self::Dispatch => write!(f, "dispatch"),
            Self::Spawn => write!(f, "spawn"),
            Self::PopulationTiming => write!(f, "population-timing"),
        }
    }
}
