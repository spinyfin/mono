//! What the service publishes, and when.
//!
//! Streamed text arrives about 20 times a second, and the app cannot render
//! each delta (coordinator ACP spike, S14: bringing the view up to date cost
//! 17.6 ms a delta on average and grew with the answer, so every-delta
//! rendering used more time than the stream lasted). Deltas are therefore
//! held and merged for [`DELTA_PUBLISH_INTERVAL`] before they go out. Anything
//! else (a completed item, a state change) is not held back, but it never
//! overtakes a delta that was queued before it.

use std::time::{Duration, Instant};

use boss_protocol::{
    CoordinatorChatConnectionState, CoordinatorChatItem, CoordinatorChatMessageDelta, CoordinatorChatTopicUpdate,
    TopicEventPayload,
};

/// How long streamed text is held and merged before it is published.
///
/// The spike measured 620 deltas at a median 47 ms apart (about 20 a second)
/// and recommended publishing every 100 to 150 ms, which brings that to 7 to
/// 10 updates a second. This is the middle of that range.
pub const DELTA_PUBLISH_INTERVAL: Duration = Duration::from_millis(125);

/// One update for the app, tagged with the generation it belongs to.
///
/// This is the body of a [`TopicEventPayload::CoordinatorChat`] push.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatPush {
    pub generation: String,
    pub update: CoordinatorChatTopicUpdate,
}

impl From<ChatPush> for TopicEventPayload {
    fn from(push: ChatPush) -> Self {
        TopicEventPayload::CoordinatorChat {
            generation: push.generation,
            update: push.update,
        }
    }
}

/// Updates waiting to be published, in the order they happened.
#[derive(Debug)]
pub(super) struct PushQueue {
    interval: Duration,
    pending: Vec<CoordinatorChatTopicUpdate>,
    /// When the queue must next be flushed. `None` when it is empty.
    due: Option<Instant>,
}

impl PushQueue {
    pub(super) fn new(interval: Duration) -> Self {
        Self {
            interval,
            pending: Vec::new(),
            due: None,
        }
    }

    /// When the queue next has to be flushed, if it holds anything.
    pub(super) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Queues streamed text. It merges into the delta before it when that one
    /// is for the same message and ends where this one starts, and waits for
    /// the publish interval unless something else is already waiting.
    pub(super) fn push_delta(&mut self, delta: CoordinatorChatMessageDelta, now: Instant) {
        if let Some(CoordinatorChatTopicUpdate::Delta { delta: last }) = self.pending.last_mut()
            && last.item_id == delta.item_id
            && last.offset + last.text.len() == delta.offset
        {
            last.text.push_str(&delta.text);
            last.seq = delta.seq;
            return;
        }
        self.pending.push(CoordinatorChatTopicUpdate::Delta { delta });
        self.due.get_or_insert(now + self.interval);
    }

    /// Queues a completed or changed item, due at once. An item already
    /// waiting under the same id is replaced by this newer copy where it
    /// stands.
    pub(super) fn push_item(&mut self, item: CoordinatorChatItem, now: Instant) {
        let waiting = self.pending.iter_mut().find_map(|update| match update {
            CoordinatorChatTopicUpdate::Item { item: waiting } if waiting.id == item.id => Some(waiting),
            _ => None,
        });
        match waiting {
            Some(waiting) => *waiting = item,
            None => self.pending.push(CoordinatorChatTopicUpdate::Item { item }),
        }
        self.due = Some(now);
    }

    /// Queues a state change, due at once.
    pub(super) fn push_state(&mut self, state: CoordinatorChatConnectionState, now: Instant) {
        self.pending.push(CoordinatorChatTopicUpdate::State { state });
        self.due = Some(now);
    }

    /// Takes everything queued, in order.
    pub(super) fn take(&mut self) -> Vec<CoordinatorChatTopicUpdate> {
        self.due = None;
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use boss_protocol::{CoordinatorChatItemKind, CoordinatorChatMessageItem, CoordinatorChatMessageRole};

    use super::*;

    fn delta(item_id: &str, seq: u64, offset: usize, text: &str) -> CoordinatorChatMessageDelta {
        CoordinatorChatMessageDelta {
            item_id: item_id.to_owned(),
            seq,
            offset,
            text: text.to_owned(),
        }
    }

    fn item(id: &str, text: &str) -> CoordinatorChatItem {
        CoordinatorChatItem {
            id: id.to_owned(),
            seq: 1,
            updated_seq: 1,
            ts_ms: 0,
            kind: CoordinatorChatItemKind::Message(CoordinatorChatMessageItem {
                role: CoordinatorChatMessageRole::Coordinator,
                text: text.to_owned(),
                client_message_id: None,
                message_id: None,
                sender: None,
            }),
        }
    }

    #[test]
    fn contiguous_deltas_merge_and_wait_for_the_interval() {
        let start = Instant::now();
        let mut queue = PushQueue::new(DELTA_PUBLISH_INTERVAL);
        queue.push_delta(delta("msg-1", 3, 0, "Hel"), start);
        queue.push_delta(delta("msg-1", 4, 3, "lo "), start + Duration::from_millis(40));
        queue.push_delta(delta("msg-1", 5, 6, "there"), start + Duration::from_millis(80));
        assert_eq!(
            queue.due(),
            Some(start + DELTA_PUBLISH_INTERVAL),
            "the interval runs from the first held delta, not the latest"
        );
        // Offsets 0..3 and 3..6 merged; 6.. continues the merged delta too.
        assert_eq!(
            queue.take(),
            vec![CoordinatorChatTopicUpdate::Delta {
                delta: delta("msg-1", 5, 0, "Hello there")
            }]
        );
        assert!(queue.due().is_none());
    }

    #[test]
    fn a_gap_or_another_message_starts_a_new_delta() {
        let now = Instant::now();
        let mut queue = PushQueue::new(DELTA_PUBLISH_INTERVAL);
        queue.push_delta(delta("msg-1", 3, 0, "abc"), now);
        queue.push_delta(delta("msg-1", 5, 9, "xyz"), now);
        queue.push_delta(delta("msg-7", 6, 0, "other"), now);
        assert_eq!(
            queue.take().len(),
            3,
            "a merge never papers over a gap the app can detect"
        );
    }

    #[test]
    fn an_item_is_due_at_once_and_keeps_its_place_after_earlier_deltas() {
        let start = Instant::now();
        let mut queue = PushQueue::new(DELTA_PUBLISH_INTERVAL);
        queue.push_delta(delta("msg-1", 3, 0, "Hi"), start);
        queue.push_item(item("tool-4", "x"), start + Duration::from_millis(10));
        assert_eq!(queue.due(), Some(start + Duration::from_millis(10)));
        let updates = queue.take();
        assert!(matches!(updates[0], CoordinatorChatTopicUpdate::Delta { .. }));
        assert!(matches!(&updates[1], CoordinatorChatTopicUpdate::Item { item } if item.id == "tool-4"));
    }

    #[test]
    fn a_newer_copy_of_a_waiting_item_replaces_it_in_place() {
        let now = Instant::now();
        let mut queue = PushQueue::new(DELTA_PUBLISH_INTERVAL);
        queue.push_item(item("tool-4", "running"), now);
        queue.push_item(item("tool-9", "other"), now);
        queue.push_item(item("tool-4", "done"), now);
        let updates = queue.take();
        assert_eq!(updates.len(), 2);
        match &updates[0] {
            CoordinatorChatTopicUpdate::Item { item } => {
                assert_eq!(item.id, "tool-4");
                assert!(matches!(&item.kind, CoordinatorChatItemKind::Message(m) if m.text == "done"));
            }
            other => panic!("expected the first item, got {other:?}"),
        }
    }

    #[test]
    fn a_state_change_is_due_at_once() {
        let now = Instant::now();
        let mut queue = PushQueue::new(DELTA_PUBLISH_INTERVAL);
        queue.push_state(CoordinatorChatConnectionState::default(), now);
        assert_eq!(queue.due(), Some(now));
    }

    #[test]
    fn a_push_becomes_the_topic_payload() {
        let payload: TopicEventPayload = ChatPush {
            generation: "g".to_owned(),
            update: CoordinatorChatTopicUpdate::Delta {
                delta: delta("msg-1", 1, 0, "a"),
            },
        }
        .into();
        assert!(matches!(payload, TopicEventPayload::CoordinatorChat { generation, .. } if generation == "g"));
    }
}
