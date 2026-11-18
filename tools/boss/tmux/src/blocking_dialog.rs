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
            "BlockingDialogOnScreen: pane input refused because a blocking dialog was on screen; matched marker {}",
            serde_json::to_string(&self.marker).map_err(|_| std::fmt::Error)?
        )
    }
}

impl std::error::Error for BlockingDialogOnScreen {}

impl BlockingDialogOnScreen {
    /// Restore the typed refusal from the host's spawn result.
    pub fn from_rejection_reason(reason: &str) -> Option<Self> {
        let marker = reason.strip_prefix(
            "BlockingDialogOnScreen: pane input refused because a blocking dialog was on screen; matched marker ",
        )?;
        serde_json::from_str(marker).ok().map(|marker| Self { marker })
    }

    /// Prefix the host writes in front of a deliver-path error.
    pub const DELIVERY_ATTEMPTED_PREFIX: &'static str = "deliver attempted: ";

    /// Host-side envelope for a refused or uncertain pane write.
    pub fn delivery_attempt_reason(error: &impl std::fmt::Display) -> String {
        format!("{}{error:#}", Self::DELIVERY_ATTEMPTED_PREFIX)
    }

    /// Recognize the host delivery envelope for a refused pane write.
    pub fn is_delivery_rejection(reason: &str) -> bool {
        reason
            .strip_prefix(Self::DELIVERY_ATTEMPTED_PREFIX)
            .is_some_and(|rest| rest.starts_with("BlockingDialogOnScreen:"))
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

const BOX_GLYPHS: [char; 4] = ['│', '┃', '║', '|'];

fn normalize_dialog_line(line: &str) -> &str {
    line.trim()
        .trim_start_matches(BOX_GLYPHS)
        .trim_end_matches(BOX_GLYPHS)
        .trim()
        // `❯` marks the selected choice in Claude's and Codex's menus, `>` in
        // Antigravity's.
        .trim_start_matches(['❯', '>'])
        .trim()
}

/// Modal choices are adjacent to the question and cancellation footer.
/// Ordinary transcript prose and quoted questions do not establish a dialog.
fn dialog_contains(visible: &str, marker: &str) -> bool {
    footer_dialog_contains(visible, marker) || selection_menu_contains(visible, marker)
}

/// The selected row of Antigravity's startup menus. Its trust dialog and its
/// `Settings Error` screen carry no `esc to cancel` footer and no numbered
/// choices; the answer is the `>`-marked row among unnumbered ones.
const SELECTION_MENU_CHOICES: [&str; 2] = ["Yes, I trust this folder", "Exit and fix manually"];

fn selection_menu_contains(visible: &str, marker: &str) -> bool {
    let mut lines: Vec<_> = visible.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    // The live menu owns the bottom of the screen: its selected row is the
    // last `>` row, with no composer (`>` or `❯`) after it. A transcript copy
    // is followed by the real composer.
    let Some(selected) = lines.iter().rposition(|line| {
        let row = line.trim().trim_start_matches(BOX_GLYPHS).trim();
        row.starts_with('>') || row.contains('❯')
    }) else {
        return false;
    };
    if !lines[selected]
        .trim()
        .trim_start_matches(BOX_GLYPHS)
        .trim()
        .starts_with('>')
        || !SELECTION_MENU_CHOICES.contains(&normalize_dialog_line(lines[selected]))
    {
        return false;
    }
    lines[..selected].iter().enumerate().any(|(index, line)| {
        let question = normalize_dialog_line(line).trim_start_matches('⚠').trim();
        !marker.is_empty() && lines.len() - index <= 12 && question.starts_with(marker)
    })
}

fn footer_dialog_contains(visible: &str, marker: &str) -> bool {
    let lines: Vec<_> = visible.lines().collect();
    // An active modal owns the footer. Status chrome below that footer
    // (mode line, bypass banner) must not hide it. A transcript example
    // above a later composer is ignored because its footer is not the
    // last `esc to cancel` on screen.
    let Some(footer) = lines
        .iter()
        .rposition(|line| normalize_dialog_line(line).to_lowercase().contains("esc to cancel"))
    else {
        return false;
    };
    // Status chrome under the modal is fine. A later composer (`❯ …`)
    // means the footer belongs to a transcript copy, not the live surface.
    if lines[footer + 1..].iter().any(|line| {
        let trimmed = line
            .trim()
            .trim_start_matches(BOX_GLYPHS)
            .trim_end_matches(BOX_GLYPHS)
            .trim();
        !trimmed.is_empty() && (trimmed.starts_with('❯') || trimmed.contains('❯') || trimmed.starts_with('>'))
    }) {
        return false;
    }
    lines[..=footer].iter().enumerate().any(|(index, line)| {
        let question = normalize_dialog_line(line);
        let tail = &lines[index.saturating_sub(2)..=footer];
        footer - index < 12
            && question.starts_with(marker)
            && ((tail.iter().any(|line| normalize_dialog_line(line).starts_with("1."))
                && tail.iter().any(|line| normalize_dialog_line(line).starts_with("2.")))
                || (tail.iter().any(|line| normalize_dialog_line(line) == "No, exit")
                    && tail.iter().any(|line| {
                        let option = normalize_dialog_line(line);
                        option == "Yes, I trust this folder" || option == "Yes, I accept"
                    })
                    && tail.iter().any(|line| line.trim().contains('❯'))))
    })
}
