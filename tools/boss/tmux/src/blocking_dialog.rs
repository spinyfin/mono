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

impl Tmux {
    /// Scope the driver policy to this handle, never to other sessions sharing its server.
    pub fn with_blocking_dialog_markers(mut self, markers: Vec<String>) -> Self {
        self.blocking_dialog_markers = markers;
        self
    }

    /// Fail closed on capture errors. Only the visible screen counts, not scrollback.
    pub async fn check_blocking_dialog(&self, session: &str) -> anyhow::Result<()> {
        if self.blocking_dialog_markers.is_empty() {
            return Ok(());
        }
        let visible = self.capture_pane(session).await?;
        if let Some(marker) = self
            .blocking_dialog_markers
            .iter()
            .find(|marker| !marker.is_empty() && visible.contains(marker.as_str()))
        {
            return Err(BlockingDialogOnScreen { marker: marker.clone() }.into());
        }
        Ok(())
    }
}
