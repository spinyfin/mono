//! Finish the immediate first interval tick before a test advances its clock.
use super::*;

#[derive(Default)]
pub struct Startup {
    pub pulse_replied: AtomicBool,
    pub received: AtomicU64,
}

impl Peer {
    pub(super) async fn settle_initial_pulse(&self) {
        // Tokio intervals tick immediately, even with a one-hour period. A
        // reply queued on the peer is insufficient: the hub must ingest it
        // before a test can safely advance past the pulse's one-second timeout.
        crate::timing::guarded(async {
            // Cross the timer wheel's rounded initial tick while remaining
            // safely inside an already-started pulse's one-second deadline.
            tokio::time::advance(Duration::from_millis(50)).await;
            crate::timing::condition(|| self.liveness.pulse_replied.load(Ordering::SeqCst)).await;
            let sequence = self.next_record.fetch_add(1, Ordering::SeqCst);
            self.tx
                .send(frame_of(spoke_frame::Body::Record(Record {
                    sequence,
                    observed: Some(Stamp {
                        boot_id: "boot".into(),
                        ..Default::default()
                    }),
                    run: None,
                    body: Some(record::Body::Capacity(Capacity {
                        generation: 1,
                        ..Default::default()
                    })),
                })))
                .await
                .unwrap();
            // Frames on this stream are ordered, so this receipt proves that
            // the preceding PulseReply has cleared the outstanding challenge.
            crate::timing::condition(|| self.liveness.received.load(Ordering::SeqCst) >= sequence).await;
        })
        .await;
    }
}

#[tokio::test]
async fn frozen_liveness_survives_advancing_past_the_initial_pulse_deadline() {
    crate::timing::hold();
    crate::timing::guarded(async {
        let fx = Fixture::with_frozen_liveness(crate::short(300)).await;
        fx.ready(HOST);
        let adapter = fx.adapter(HOST, "exec_1").await.unwrap();
        let peer = fx.spoke(Box::new(crate::standard), None, 0).await;
        // Exceed the unchanged one-second pulse timeout immediately after
        // connecting, before any command has supplied another round trip.
        tokio::time::advance(Duration::from_secs(2)).await;
        crate::lease(&adapter).await;
        assert_eq!(peer.commands().len(), 1);
    })
    .await;
}
