//! Capture actual engine boundaries without changing the wire protocol or
//! using wall-clock timestamps. The handler span correlates every sample.
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

#[derive(Clone, Default)]
pub struct Boundaries(Arc<Mutex<Vec<(tracing::Id, String, Instant)>>>);

#[derive(Default)]
struct Boundary(String);

impl Visit for Boundary {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "boundary" {
            self.0 = value.to_owned();
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Boundaries {
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let at = Instant::now();
        if event.metadata().target() != "work_write_timing" {
            return;
        }
        let Some(scope) = ctx.event_scope(event) else { return };
        let spans: Vec<_> = scope.collect();
        let Some(handler) = spans.iter().find(|span| span.metadata().name() == "update_work_item") else {
            return;
        };
        let mut boundary = Boundary::default();
        event.record(&mut boundary);
        // Reconcile can also commit; only the project's own transaction
        // supplies the commit boundary for this measurement.
        if boundary.0 == "commit" && !spans.iter().any(|span| span.metadata().name() == "project_transaction") {
            return;
        }
        self.0.lock().unwrap().push((handler.id(), boundary.0, at));
    }
}

impl Boundaries {
    pub fn install() -> Self {
        let capture = Self::default();
        tracing_subscriber::registry().with(capture.clone()).init();
        capture
    }

    pub fn report(&self, started: Instant, received: Instant, children: usize) {
        let samples = std::mem::take(&mut *self.0.lock().unwrap());
        let expected = ["handler_entry", "transaction_begin", "commit", "published"];
        assert_eq!(
            samples.iter().map(|(_, name, _)| name.as_str()).collect::<Vec<_>>(),
            expected
        );
        assert!(
            samples.iter().all(|(id, _, _)| *id == samples[0].0),
            "boundaries must belong to one request span"
        );
        let stamps: Vec<_> = samples.iter().map(|(_, _, at)| *at).collect();
        assert!(started <= stamps[0] && stamps[3] <= received);
        assert!(stamps.windows(2).all(|pair| pair[0] <= pair[1]));
        let ms = |a: Instant, b: Instant| b.duration_since(a).as_secs_f64() * 1000.0;
        eprintln!(
            "archive[N={children}] issue_to_entry={:.3}ms entry_to_commit={:.3}ms transaction={:.3}ms commit_to_publish={:.3}ms publish_to_reply={:.3}ms",
            ms(started, stamps[0]),
            ms(stamps[0], stamps[2]),
            ms(stamps[1], stamps[2]),
            ms(stamps[2], stamps[3]),
            ms(stamps[3], received),
        );
    }
}
