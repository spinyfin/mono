//! Pane text for the read-only remote pane viewer (design §2.8).
//!
//! A viewer sees a bounded text snapshot, never a terminal byte stream: the
//! pane is read with a plain `capture-pane -p`, the token on the session is
//! checked first so a sibling or replacement session is never read, and the
//! result is cut to the most recent lines that fit the subscriber's byte
//! budget. Nothing here can write to the pane.
use std::future::Future;

use boss_tmux::{PaneProbe, Tmux};

/// What one read of a worker's pane found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneRead {
    /// The pane's visible text. A pane that already exited still has text.
    Text(String),
    /// The session is positively absent, or answers to another token.
    Gone,
    /// The question could not be answered; never evidence either way.
    Unreadable(String),
}

/// Where pane text comes from. A trait so the subscription loop is testable
/// without a tmux server.
pub trait PaneSource: Send + Sync {
    fn read(&self, session: &str, spawn_token: &str) -> impl Future<Output = PaneRead> + Send;
}

/// Reads panes from the spoke's private tmux server.
pub struct TmuxPanes(pub Tmux);

impl PaneSource for TmuxPanes {
    async fn read(&self, session: &str, spawn_token: &str) -> PaneRead {
        // Exact match: a bare session name falls back to prefix matching.
        let target = format!("={session}:");
        match boss_tmux::probe_pane(&self.0, &target, spawn_token).await {
            PaneProbe::Absent | PaneProbe::ForeignToken => PaneRead::Gone,
            PaneProbe::Unreadable { stage, error } => PaneRead::Unreadable(format!("{stage}: {error:#}")),
            PaneProbe::Alive | PaneProbe::Dead { .. } => match self.0.capture_pane(&target).await {
                Ok(text) => PaneRead::Text(text),
                Err(error) => PaneRead::Unreadable(format!("capture: {error:#}")),
            },
        }
    }
}

/// Cut `raw` to a snapshot of at most `max_bytes`: tmux pads the pane to its
/// height with blank lines, which carry no information; when the text is still
/// too long the *newest* lines are kept, starting on a line boundary. The flag
/// says whether anything older was dropped.
pub fn bound_snapshot(raw: &str, max_bytes: usize) -> (String, bool) {
    let mut text = raw.replace("\r\n", "\n");
    text.truncate(text.trim_end().len());
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    // Begin at the next line so a cut never shows half a line, unless the tail
    // is one very long line, which is kept as far as it fits.
    if let Some(newline) = text[start..].find('\n')
        && start + newline + 1 < text.len()
    {
        start += newline + 1;
    }
    (text[start..].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_is_dropped_and_short_text_is_whole() {
        assert_eq!(bound_snapshot("a\nb\n\n\n  \n", 64), ("a\nb".to_owned(), false));
        assert_eq!(bound_snapshot("", 64), (String::new(), false));
    }

    #[test]
    fn the_newest_whole_lines_survive_a_cut() {
        let text = "one\ntwo\nthree\nfour";
        let (cut, truncated) = bound_snapshot(text, 12);
        assert!(truncated);
        assert_eq!(cut, "three\nfour");
        assert!(cut.len() <= 12);
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let text = "é".repeat(10);
        for max in 1..=20 {
            let (cut, truncated) = bound_snapshot(&text, max);
            assert!(cut.len() <= max, "{max}: {}", cut.len());
            assert_eq!(truncated, max < text.len());
            assert!(cut.chars().all(|c| c == 'é'));
        }
    }

    #[test]
    fn one_enormous_line_keeps_its_tail() {
        let (cut, truncated) = bound_snapshot(&"x".repeat(100), 10);
        assert!(truncated);
        assert_eq!(cut, "x".repeat(10));
    }

    #[test]
    fn carriage_returns_do_not_survive() {
        assert_eq!(bound_snapshot("a\r\nb\r\n", 64).0, "a\nb");
    }
}
