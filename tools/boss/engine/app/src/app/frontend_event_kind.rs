//! Bounded push labels without serializing event payloads.
use boss_protocol::FrontendEvent;

pub(super) fn kind(event: &FrontendEvent) -> &'static str {
    event.variant_label()
}
