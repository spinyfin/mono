//! What a kanban drag-and-drop *means*.
//!
//! The work board is a rendering of engine state, not a store of it. A row's
//! column and — in the Done column — its group are both derived from
//! `status`, `autostart`, `blocked_reason` and `merge_queue_state`; nothing
//! about the layout is written down anywhere. So a drop cannot be turned into
//! a status by looking at the drop target alone: the *same* drop target is a
//! reorder for a card that was already there and a transition for one that
//! arrived from somewhere else.
//!
//! This crate owns that decision. It derives a row's current
//! [`BoardPosition`] and resolves an observed [`BoardDropTarget`] against it
//! into exactly one [`DropResolution`]. It holds no I/O and no engine
//! handles, so the whole gesture matrix is unit-testable.
//!
//! # Why it lives in the engine
//!
//! A column does not determine its rows' statuses. Done holds both `done`
//! rows and `in_review` rows whose PR is in a merge queue (or has Merge When
//! Ready armed); Doing holds `active` rows and `todo` rows queued to
//! dispatch; Review holds `in_review` rows and review-phase-`blocked` ones.
//! So the target column alone cannot name a status, and the app — which sees
//! only the layout — cannot supply the missing half. Resolving a drop
//! requires the row's own derived position, which only the engine has.
//!
//! # The rule
//!
//! **A drop that does not leave the card's current board position never
//! changes status.** Everything else follows from that plus one refusal:
//! [`BoardGroup::Merging`] is engine-owned — a row is in it because the
//! engine observed its PR in a merge queue, so no drag can put a row there.

use boss_protocol::{BoardColumn, BoardDropTarget, BoardGroup, TaskStatus};

/// The board-relevant slice of a work item. Deliberately a value type over
/// primitive fields rather than a `&Task`: the derivation depends on exactly
/// these four columns, and naming them keeps it obvious when a future field
/// starts affecting board layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRow {
    pub status: TaskStatus,
    /// `true` for a row queued to dispatch as soon as a slot frees. Such a
    /// row is `todo` but renders in Doing, which is why Backlog and Doing can
    /// both map to `todo`.
    pub autostart: bool,
    /// `tasks.blocked_reason`, `None` for any non-`blocked` row (the DB
    /// enforces that invariant on every write path).
    pub blocked_reason: Option<String>,
    /// `tasks.merge_queue_state` — `Some("queued")` while the PR is in a
    /// merge queue, `Some("auto_merge_enabled")` with Merge When Ready armed,
    /// `None` otherwise. Engine-owned: only the merge/queue pollers write it.
    pub merge_queue_state: Option<String>,
}

impl BoardRow {
    /// `true` when the row is blocked for a review-phase reason: it has an
    /// open PR and the block is transient (conflict resolution, a CI fix, or
    /// review feedback in flight). Those render in Review, not Backlog.
    ///
    /// Mirror of `WorkTask.isReviewPhaseBlocked` in the macOS client.
    fn is_review_phase_blocked(&self) -> bool {
        matches!(
            self.blocked_reason.as_deref(),
            Some("merge_conflict") | Some("ci_failure") | Some("ci_failure_exhausted") | Some("review_feedback")
        )
    }

    /// `true` when the row renders in the Done column's "Merging" group.
    ///
    /// Gated on `in_review` for the same reason the client gates it: a
    /// just-merged row can briefly carry a stale `merge_queue_state` if the
    /// merge transition raced the poller's next dequeue write, and without
    /// the gate it would look like an in-flight merge forever.
    ///
    /// Mirror of `WorkTask.isInMergingSection`.
    fn is_merging(&self) -> bool {
        self.status == TaskStatus::InReview && self.merge_queue_state.is_some()
    }

    /// Where this row currently renders.
    ///
    /// Mirror of `WorkTask.boardColumn` + `ChatViewModel.computeWorkSections`
    /// in the macOS client. The client keeps its own copy because it must lay
    /// out cards without a round trip; this one is the authority for deciding
    /// what a gesture meant. Any change to one is a change to both.
    pub fn position(&self) -> BoardPosition {
        match self.status {
            TaskStatus::Active => BoardPosition::column(BoardColumn::Doing),
            TaskStatus::InReview if self.is_merging() => BoardPosition::group(BoardColumn::Done, BoardGroup::Merging),
            TaskStatus::InReview => BoardPosition::column(BoardColumn::Review),
            TaskStatus::Done => BoardPosition::group(BoardColumn::Done, BoardGroup::Completed),
            TaskStatus::Todo if self.autostart => BoardPosition::column(BoardColumn::Doing),
            TaskStatus::Blocked if self.is_review_phase_blocked() => BoardPosition::column(BoardColumn::Review),
            // `todo` without autostart, `blocked` for a non-review reason,
            // and the terminal-but-not-done statuses. The last two are not
            // normally rendered at all (archived/cancelled rows are filtered
            // out of the board), but they need a position so a drop on a
            // stale card resolves rather than panicking.
            _ => BoardPosition::column(BoardColumn::Backlog),
        }
    }
}

/// A row's location on the board: its column, and its group within that
/// column when the column has groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardPosition {
    pub column: BoardColumn,
    pub group: Option<BoardGroup>,
}

impl BoardPosition {
    fn column(column: BoardColumn) -> Self {
        Self { column, group: None }
    }

    fn group(column: BoardColumn, group: BoardGroup) -> Self {
        Self {
            column,
            group: Some(group),
        }
    }

    pub fn label(&self) -> String {
        match self.group {
            Some(group) => format!("{}/{}", self.column.as_str(), group.as_str()),
            None => self.column.as_str().to_owned(),
        }
    }
}

/// The single effect a resolved drop is allowed to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropResolution {
    /// The card did not leave its board position. Status-preserving by
    /// definition: apply no patch at all. `reason` explains which sense of
    /// "did not leave" applied, for the forensic log.
    Reorder { reason: ReorderReason },
    /// The drop crossed into a position that implies a different status.
    SetStatus(TaskStatus),
    /// Doing → Backlog for a row that is queued to dispatch but has not
    /// started (`todo` + `autostart`). Its status is already `todo`, so the
    /// gesture is expressed by clearing `autostart`, which is what moves the
    /// card to Backlog.
    ClearAutostart,
    /// The drop names no meaningful gesture. Mutate nothing and explain why,
    /// rather than picking the nearest plausible transition.
    Refused { message: String },
}

/// Which flavour of "the card stayed put" a [`DropResolution::Reorder`] was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorderReason {
    /// The drop landed on the same group the card was already in.
    SameGroup,
    /// The drop landed on the card's own column without naming a group — the
    /// column's padding, or a column with no groups. An unqualified drop
    /// carries no more intent than "somewhere in this column", and the card
    /// is already in this column, so there is nothing to apply.
    SameColumnUnqualified,
}

/// Resolve an observed drop against the row's own derived position.
///
/// Returns exactly one effect. The ordering of the arms is the contract:
///
/// 1. **Landed where it already was** → [`DropResolution::Reorder`]. Checked
///    first, so no later arm can turn a reorder into a transition. Covers
///    both "same group" and "same column, no group named".
/// 2. **Target is the Merging group** → refused. A row enters Merging
///    because the engine observed its PR in a merge queue or with Merge When
///    Ready armed; a drag cannot assert that.
/// 3. **Merging → Review** → refused. Both are `in_review`, so there is no
///    status to change; the gesture reads as "take this out of the merge
///    queue", which is a merge-queue operation, not a board move.
/// 4. **Everything else** → the target position's status, except the one
///    case where the target's status equals the row's own
///    ([`DropResolution::ClearAutostart`]).
///
/// A group named for a column that has none is discarded before any of that
/// runs: Done is the only column the board groups, so a group qualifier on
/// any other column describes a target that does not exist. The wire is
/// untrusted, and degrading such a target to an unqualified drop on the named
/// column keeps every arm below reasoning about positions the board can
/// actually render.
pub fn resolve_drop(row: &BoardRow, target: BoardDropTarget) -> DropResolution {
    let current = row.position();
    let target_group = match target.column {
        BoardColumn::Done => target.group,
        _ => None,
    };

    if current.column == target.column {
        match target_group {
            None => {
                return DropResolution::Reorder {
                    reason: ReorderReason::SameColumnUnqualified,
                };
            }
            Some(group) if current.group == Some(group) => {
                return DropResolution::Reorder {
                    reason: ReorderReason::SameGroup,
                };
            }
            Some(_) => {}
        }
    }

    if target_group == Some(BoardGroup::Merging) {
        return DropResolution::Refused {
            message: "the Done column's \"Merging\" group is engine-owned: a card appears there \
                      once Boss observes its PR in a merge queue or with Merge When Ready armed. \
                      Drop it on a completion group to mark it done, or use the PR's merge \
                      controls to change how it merges."
                .to_owned(),
        };
    }

    if current.group == Some(BoardGroup::Merging) && target.column == BoardColumn::Review {
        return DropResolution::Refused {
            message: "this card's PR is in flight (merge queue or Merge When Ready), which is why \
                      it sits in Done ▸ Merging rather than Review — its status is already \
                      `in_review`, so there is nothing for this move to change. Cancel the queue \
                      entry or disarm Merge When Ready to bring it back to Review."
                .to_owned(),
        };
    }

    let target_status = status_for_column(target.column);
    if target_status == row.status {
        // The only way to reach this: Doing → Backlog for a `todo` +
        // `autostart` row, the one pair of columns that map to the same
        // status. Every other row whose status matches its target column's
        // is already in that column, so the reorder arm claimed it first —
        // which is only true because a group named for a groupless column
        // was discarded above rather than being allowed to skip that arm.
        // The card is visibly moving, so a bare no-op would look like a
        // dropped gesture; clearing `autostart` is what parks it in Backlog.
        return DropResolution::ClearAutostart;
    }
    DropResolution::SetStatus(target_status)
}

/// The status a column asserts for any card dropped into it from elsewhere.
///
/// Only the Done column has groups, and both of its groups resolve through
/// arms above this point (Merging is refused; Completed is `done`), so the
/// column alone is enough here.
fn status_for_column(column: BoardColumn) -> TaskStatus {
    match column {
        BoardColumn::Backlog => TaskStatus::Todo,
        BoardColumn::Doing => TaskStatus::Active,
        BoardColumn::Review => TaskStatus::InReview,
        BoardColumn::Done => TaskStatus::Done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(status: TaskStatus) -> BoardRow {
        BoardRow {
            status,
            autostart: false,
            blocked_reason: None,
            merge_queue_state: None,
        }
    }

    /// An `in_review` chore whose PR is in the Trunk merge queue — the shape
    /// that renders in Done ▸ Merging.
    fn merging_row() -> BoardRow {
        BoardRow {
            merge_queue_state: Some("queued".to_owned()),
            ..row(TaskStatus::InReview)
        }
    }

    fn target(column: BoardColumn, group: Option<BoardGroup>) -> BoardDropTarget {
        BoardDropTarget::new(column, group)
    }

    // ── position derivation ─────────────────────────────────────────────

    #[test]
    fn merging_row_renders_in_done_merging_not_review() {
        assert_eq!(
            merging_row().position(),
            BoardPosition::group(BoardColumn::Done, BoardGroup::Merging)
        );
    }

    #[test]
    fn done_row_renders_in_done_completed() {
        assert_eq!(
            row(TaskStatus::Done).position(),
            BoardPosition::group(BoardColumn::Done, BoardGroup::Completed)
        );
    }

    #[test]
    fn in_review_without_queue_state_renders_in_review() {
        assert_eq!(
            row(TaskStatus::InReview).position(),
            BoardPosition::column(BoardColumn::Review)
        );
    }

    #[test]
    fn done_row_with_stale_queue_state_is_not_merging() {
        // The merge transition can race the queue poller's dequeue write.
        // A `done` row carrying a leftover queue state must still bucket as
        // completed, or it would look like an in-flight merge forever.
        let stale = BoardRow {
            merge_queue_state: Some("queued".to_owned()),
            ..row(TaskStatus::Done)
        };
        assert_eq!(
            stale.position(),
            BoardPosition::group(BoardColumn::Done, BoardGroup::Completed)
        );
    }

    #[test]
    fn dispatch_pending_row_renders_in_doing() {
        let pending = BoardRow {
            autostart: true,
            ..row(TaskStatus::Todo)
        };
        assert_eq!(pending.position(), BoardPosition::column(BoardColumn::Doing));
    }

    #[test]
    fn review_phase_blocked_renders_in_review_other_blocks_in_backlog() {
        for reason in [
            "merge_conflict",
            "ci_failure",
            "ci_failure_exhausted",
            "review_feedback",
        ] {
            let blocked = BoardRow {
                blocked_reason: Some(reason.to_owned()),
                ..row(TaskStatus::Blocked)
            };
            assert_eq!(
                blocked.position(),
                BoardPosition::column(BoardColumn::Review),
                "{reason} should render in Review"
            );
        }
        let other = BoardRow {
            blocked_reason: Some("waiting_on_human".to_owned()),
            ..row(TaskStatus::Blocked)
        };
        assert_eq!(other.position(), BoardPosition::column(BoardColumn::Backlog));
    }

    // ── the defect this crate exists for ────────────────────────────────

    #[test]
    fn reorder_within_merging_group_does_not_complete_the_merge() {
        assert_eq!(
            resolve_drop(&merging_row(), target(BoardColumn::Done, Some(BoardGroup::Merging))),
            DropResolution::Reorder {
                reason: ReorderReason::SameGroup
            }
        );
    }

    #[test]
    fn unqualified_drop_on_done_column_from_merging_does_not_complete_the_merge() {
        // A sloppy drop that lands on the column's padding rather than on a
        // group names less intent, not more — it must not be read as
        // "complete this".
        assert_eq!(
            resolve_drop(&merging_row(), target(BoardColumn::Done, None)),
            DropResolution::Reorder {
                reason: ReorderReason::SameColumnUnqualified
            }
        );
    }

    #[test]
    fn merging_to_completion_group_is_a_real_completion() {
        assert_eq!(
            resolve_drop(&merging_row(), target(BoardColumn::Done, Some(BoardGroup::Completed))),
            DropResolution::SetStatus(TaskStatus::Done)
        );
    }

    // ── the same accident in every other column ─────────────────────────

    #[test]
    fn reorder_within_done_completion_groups_is_a_no_op() {
        for group in [None, Some(BoardGroup::Completed)] {
            assert!(matches!(
                resolve_drop(&row(TaskStatus::Done), target(BoardColumn::Done, group)),
                DropResolution::Reorder { .. }
            ));
        }
    }

    #[test]
    fn reorder_within_review_does_not_clear_a_review_phase_block() {
        // Before the engine owned this decision, a `blocked: ci_failure` row
        // reordered inside Review flipped to `in_review` — clearing the block
        // and suppressing CI re-detection on the current head sha. The
        // explicit affordance (the card popover's Move buttons) still does
        // that; a drag no longer does it by accident.
        let blocked = BoardRow {
            blocked_reason: Some("ci_failure".to_owned()),
            ..row(TaskStatus::Blocked)
        };
        assert!(matches!(
            resolve_drop(&blocked, target(BoardColumn::Review, None)),
            DropResolution::Reorder { .. }
        ));
    }

    #[test]
    fn reorder_within_backlog_does_not_clear_a_block() {
        let blocked = BoardRow {
            blocked_reason: Some("waiting_on_human".to_owned()),
            ..row(TaskStatus::Blocked)
        };
        assert!(matches!(
            resolve_drop(&blocked, target(BoardColumn::Backlog, None)),
            DropResolution::Reorder { .. }
        ));
    }

    #[test]
    fn reorder_within_doing_does_not_start_a_dispatch_pending_row() {
        let pending = BoardRow {
            autostart: true,
            ..row(TaskStatus::Todo)
        };
        assert!(matches!(
            resolve_drop(&pending, target(BoardColumn::Doing, None)),
            DropResolution::Reorder { .. }
        ));
    }

    // ── transitions that must keep working ──────────────────────────────

    #[test]
    fn backlog_to_doing_activates() {
        assert_eq!(
            resolve_drop(&row(TaskStatus::Todo), target(BoardColumn::Doing, None)),
            DropResolution::SetStatus(TaskStatus::Active)
        );
    }

    #[test]
    fn active_dragged_to_backlog_parks_via_status() {
        assert_eq!(
            resolve_drop(&row(TaskStatus::Active), target(BoardColumn::Backlog, None)),
            DropResolution::SetStatus(TaskStatus::Todo)
        );
    }

    #[test]
    fn dispatch_pending_dragged_to_backlog_clears_autostart() {
        // Both columns map to `todo`, so a status patch would be a no-op and
        // the card would snap back. Clearing autostart is the gesture.
        let pending = BoardRow {
            autostart: true,
            ..row(TaskStatus::Todo)
        };
        assert_eq!(
            resolve_drop(&pending, target(BoardColumn::Backlog, None)),
            DropResolution::ClearAutostart
        );
    }

    #[test]
    fn review_to_done_completes() {
        for group in [None, Some(BoardGroup::Completed)] {
            assert_eq!(
                resolve_drop(&row(TaskStatus::InReview), target(BoardColumn::Done, group)),
                DropResolution::SetStatus(TaskStatus::Done)
            );
        }
    }

    #[test]
    fn done_dragged_back_out_reopens() {
        assert_eq!(
            resolve_drop(&row(TaskStatus::Done), target(BoardColumn::Review, None)),
            DropResolution::SetStatus(TaskStatus::InReview)
        );
        assert_eq!(
            resolve_drop(&row(TaskStatus::Done), target(BoardColumn::Doing, None)),
            DropResolution::SetStatus(TaskStatus::Active)
        );
    }

    #[test]
    fn merging_to_doing_or_backlog_is_a_real_transition() {
        assert_eq!(
            resolve_drop(&merging_row(), target(BoardColumn::Doing, None)),
            DropResolution::SetStatus(TaskStatus::Active)
        );
        assert_eq!(
            resolve_drop(&merging_row(), target(BoardColumn::Backlog, None)),
            DropResolution::SetStatus(TaskStatus::Todo)
        );
    }

    // ── refusals ────────────────────────────────────────────────────────

    #[test]
    fn nothing_can_be_dragged_into_the_merging_group() {
        for status in [
            TaskStatus::Todo,
            TaskStatus::Active,
            TaskStatus::InReview,
            TaskStatus::Done,
        ] {
            let label = status.as_str();
            assert!(
                matches!(
                    resolve_drop(&row(status), target(BoardColumn::Done, Some(BoardGroup::Merging))),
                    DropResolution::Refused { .. }
                ),
                "{label} → Done/Merging should be refused",
            );
        }
    }

    #[test]
    fn merging_to_review_is_refused_rather_than_silently_nothing() {
        assert!(matches!(
            resolve_drop(&merging_row(), target(BoardColumn::Review, None)),
            DropResolution::Refused { .. }
        ));
    }

    #[test]
    fn a_group_named_for_a_groupless_column_is_ignored() {
        // Only Done has groups, so these targets describe a position the
        // board cannot render. The client never sends them, but the wire is
        // untrusted: each must degrade to the unqualified drop it names,
        // never slip past the reorder arm on a group mismatch.
        let pending = BoardRow {
            autostart: true,
            ..row(TaskStatus::Todo)
        };
        for group in [BoardGroup::Merging, BoardGroup::Completed] {
            assert!(
                matches!(
                    resolve_drop(&row(TaskStatus::Active), target(BoardColumn::Doing, Some(group))),
                    DropResolution::Reorder { .. }
                ),
                "active row dropped on Doing/{} must stay a reorder",
                group.as_str(),
            );
            assert!(
                matches!(
                    resolve_drop(&pending, target(BoardColumn::Backlog, Some(group))),
                    DropResolution::ClearAutostart
                ),
                "the Backlog/{} target must resolve as the plain Backlog drop it is",
                group.as_str(),
            );
            // Merging is refused only where it exists. Named against
            // Review it is discarded, leaving an ordinary transition.
            assert_eq!(
                resolve_drop(&row(TaskStatus::Todo), target(BoardColumn::Review, Some(group))),
                DropResolution::SetStatus(TaskStatus::InReview),
            );
        }
    }

    // ── the whole matrix is total ───────────────────────────────────────

    #[test]
    fn every_position_and_target_pair_resolves() {
        let rows = [
            row(TaskStatus::Todo),
            BoardRow {
                autostart: true,
                ..row(TaskStatus::Todo)
            },
            row(TaskStatus::Active),
            BoardRow {
                blocked_reason: Some("ci_failure".to_owned()),
                ..row(TaskStatus::Blocked)
            },
            BoardRow {
                blocked_reason: Some("waiting_on_human".to_owned()),
                ..row(TaskStatus::Blocked)
            },
            row(TaskStatus::InReview),
            merging_row(),
            row(TaskStatus::Done),
        ];
        let columns = [
            BoardColumn::Backlog,
            BoardColumn::Doing,
            BoardColumn::Review,
            BoardColumn::Done,
        ];
        let groups = [None, Some(BoardGroup::Merging), Some(BoardGroup::Completed)];
        for r in &rows {
            for column in columns {
                for group in groups {
                    let resolution = resolve_drop(r, target(column, group));
                    // The invariant the whole crate is for: a drop that did
                    // not leave the row's position never carries a status.
                    if r.position().column == column && (group.is_none() || r.position().group == group) {
                        assert!(
                            matches!(resolution, DropResolution::Reorder { .. }),
                            "{} dropped on {} should be a reorder, got {resolution:?}",
                            r.position().label(),
                            target(column, group).label(),
                        );
                    }
                    // Done is the only column with groups, so in every other
                    // one the qualifier carries no information and cannot
                    // make a same-column drop mean anything. Without the
                    // normalisation this is where a stray `ClearAutostart`
                    // would show up, for a card that never moved.
                    if r.position().column == column && column != BoardColumn::Done {
                        assert!(
                            matches!(resolution, DropResolution::Reorder { .. }),
                            "{} dropped on its own column, qualified {group:?}, should be a \
                             reorder — the group names nothing there — got {resolution:?}",
                            r.position().label(),
                        );
                    }
                }
            }
        }
    }
}
