//! Fresh visible-screen checks at the last boundary before pane input.
use crate::Tmux;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingDialogOnScreen {
    pub marker: String,
}

impl std::fmt::Display for BlockingDialogOnScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BlockingDialogOnScreen: pane input refused because a blocking dialog was on screen; matched marker {:?}",
            self.marker
        )
    }
}

impl std::error::Error for BlockingDialogOnScreen {}

impl BlockingDialogOnScreen {
    /// Recognize the host delivery envelope for a refused pane write.
    pub fn is_delivery_rejection(reason: &str) -> bool {
        reason.starts_with("deliver attempted: BlockingDialogOnScreen:")
    }
}

impl Tmux {
    /// Scope the driver policy to this handle, never to other sessions sharing its server.
    pub fn with_blocking_dialog_markers(mut self, markers: Vec<String>) -> Self {
        self.blocking_dialog_markers = markers;
        self
    }

    /// Fail closed on capture errors. Only the visible screen counts, not scrollback.
    pub async fn check_blocking_dialog(&self, session: &str) -> anyhow::Result<()> {
        self.check_blocking_dialog_excluding(session, "").await
    }

    pub(crate) async fn check_blocking_dialog_excluding(&self, session: &str, payload: &str) -> anyhow::Result<()> {
        if self.blocking_dialog_markers.is_empty() {
            return Ok(());
        }
        let capture = self.capture_pane(session).await?;
        // Remove only the contiguous text just written to the composer, never
        // individual matching lines from a modal that appeared during delivery.
        let visible = if payload.contains('\n') {
            capture.replacen(&format!("❯ {payload}"), "", 1)
        } else {
            capture
        };
        if let Some(marker) = self
            .blocking_dialog_markers
            .iter()
            .find(|marker| !marker.is_empty() && dialog_contains(&visible, marker))
        {
            return Err(BlockingDialogOnScreen { marker: marker.clone() }.into());
        }
        Ok(())
    }
}

/// Modal choices are adjacent to the question and cancellation footer.
/// Ordinary transcript prose and quoted questions do not establish a dialog.
fn dialog_contains(visible: &str, marker: &str) -> bool {
    let lines: Vec<_> = visible.lines().collect();
    // An active modal owns the footer. A transcript example above a composer
    // or later output must not be treated as the current input surface.
    let Some(footer) = lines.iter().rposition(|line| !line.trim().is_empty()) else {
        return false;
    };
    if !lines[footer].to_lowercase().contains("esc to cancel") {
        return false;
    }
    lines[..=footer].iter().enumerate().any(|(index, line)| {
        let question = line.trim().trim_start_matches('❯').trim();
        let tail = &lines[index.saturating_sub(2)..=footer];
        index <= footer
            && footer - index < 12
            && question.starts_with(marker)
            && ((tail
                .iter()
                .any(|line| line.trim().trim_start_matches('❯').trim().starts_with("1."))
                && tail
                    .iter()
                    .any(|line| line.trim().trim_start_matches('❯').trim().starts_with("2.")))
                || (tail
                    .iter()
                    .any(|line| line.trim().trim_start_matches('❯').trim() == "No, exit")
                    && tail.iter().any(|line| {
                        let option = line.trim().trim_start_matches('❯').trim();
                        option == "Yes, I trust this folder" || option == "Yes, I accept"
                    })
                    && tail.iter().any(|line| line.trim().starts_with('❯'))))
    })
}
