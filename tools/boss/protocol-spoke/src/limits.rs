//! Default size and rate limits for epoch 1.
//!
//! Limits are negotiated downward, never upward beyond local policy: a peer
//! may advertise a smaller bound in `Welcome`, and the effective bound is the
//! smaller of the two. A zero in a negotiated field means "not advertised" and
//! leaves the local default in force.

/// Maximum decoded frame (one `SpokeFrame` or `HubFrame`).
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// Maximum data bytes in one `TranscriptDelta` or `ArtifactChunk`.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;

/// Maximum combined size of `Spawn.prompt_utf8` and `Spawn.settings_json`.
pub const MAX_SPAWN_BYTES: usize = 512 * 1024;

/// Maximum size of one hook or proposal payload (tool input/response JSON,
/// canonical proposal JSON, worker-API reply JSON).
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// Bulk bytes (transcript, artifact, capture) a host may have outstanding
/// before the next chunk must wait for a receipt.
pub const MAX_INFLIGHT_BYTES: u32 = 256 * 1024;

/// Maximum text in one `PaneCapture` and the most a `Capture` may ask for.
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024;

/// Pane capture cadence and subscription lifetime when the request leaves them
/// unset.
pub const DEFAULT_CAPTURE_INTERVAL_MS: u32 = 5_000;
pub const DEFAULT_CAPTURE_TTL_SECONDS: u32 = 30;

/// A `Stop` grace period is capped; longer requests are invalid, not clamped.
pub const MAX_STOP_GRACE_MS: u32 = 30_000;

/// Length of a SHA-256 digest carried in `sha256` fields.
pub const DIGEST_BYTES: usize = 32;

/// The bounds a connection actually runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_frame_bytes: u32,
    pub max_inflight_bytes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_inflight_bytes: MAX_INFLIGHT_BYTES,
        }
    }
}

impl Limits {
    /// Applies the bounds a peer advertised (`Welcome.max_frame_bytes` and
    /// `Welcome.max_inflight_bytes`). Each field is the smaller of the local
    /// and advertised values; an advertised zero is ignored.
    pub fn negotiated(self, advertised_frame_bytes: u32, advertised_inflight_bytes: u32) -> Self {
        Self {
            max_frame_bytes: clamp_down(self.max_frame_bytes, advertised_frame_bytes),
            max_inflight_bytes: clamp_down(self.max_inflight_bytes, advertised_inflight_bytes),
        }
    }
}

fn clamp_down(local: u32, advertised: u32) -> u32 {
    if advertised == 0 { local } else { local.min(advertised) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_only_lowers_limits() {
        let local = Limits::default();
        let lowered = local.negotiated(64 * 1024, 16 * 1024);
        assert_eq!(lowered.max_frame_bytes, 64 * 1024);
        assert_eq!(lowered.max_inflight_bytes, 16 * 1024);

        let raised = local.negotiated(u32::MAX, u32::MAX);
        assert_eq!(raised, local, "a peer cannot raise a bound above local policy");
    }

    #[test]
    fn advertised_zero_keeps_the_local_default() {
        assert_eq!(Limits::default().negotiated(0, 0), Limits::default());
    }
}
