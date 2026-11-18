//! Topic names the engine publishes on, re-exported from [`crate::wire`].

pub const TOPIC_WORK_PRODUCTS: &str = "work.products";

/// Global topic carrying GitHub OAuth auth-state pushes
/// ([`crate::FrontendEvent::GitHubAuthState`]). Unlike work topics this is not
/// per-product: the engine owns a single per-host (github.com) auth state and
/// fans every transition out on this one topic. The macOS app subscribes to it
/// to render the issue-sync "GitHub account" section as the device flow
/// advances. See the OAuth device-flow design (§3 state machine, §4 RPC).
pub const TOPIC_GITHUB_AUTH: &str = "github.auth";

/// Global topic carrying engine-health pushes
/// ([`crate::FrontendEvent::EngineHealthResult`]). The engine fans every
/// health-state change (dispatch pause/resume, API key changes, etc.)
/// out on this topic so subscribed frontends update without polling.
pub const TOPIC_ENGINE_HEALTH: &str = "engine.health";

pub fn work_product_topic(product_id: &str) -> String {
    format!("work.product.{product_id}")
}

pub fn execution_topic(execution_id: &str) -> String {
    format!("executions.{execution_id}")
}

/// Per-run topic that carries probe lifecycle pushes for `run_id`.
/// Subscribers (e.g. a `bossctl probe` invocation that wants to wait
/// for the worker's reply) join this topic on the run they care about
/// and observe [`crate::FrontendEvent::ProbeReplied`] when the engine pops a
/// queued probe and watches the next Stop boundary land.
pub fn probe_topic(run_id: &str) -> String {
    format!("probes.{run_id}")
}

/// Per-artifact comment topic. Fires whenever any comment row on the
/// artifact changes (create / status flip / re-anchor); subscribers
/// refetch via `comments_list` / `comments_resolve`. Grammar:
/// `comments.artifact.<artifact_kind>:<artifact_id>`.
pub fn comment_topic(artifact_kind: &str, artifact_id: &str) -> String {
    format!("comments.artifact.{artifact_kind}:{artifact_id}")
}

/// Per-product editorial-actions topic. The engine pushes a
/// [`crate::TopicEventPayload::WorkEditorialAction`] on this topic after every
/// PreToolUse hook decision so the UI can badge product cards.
pub fn editorial_actions_topic(product_id: &str) -> String {
    format!("editorial_actions.{product_id}")
}

/// Prefix of the per-run read-only pane viewer topic.
pub const PANE_CAPTURE_TOPIC_PREFIX: &str = "pane_capture.";

/// Per-run topic carrying [`crate::TopicEventPayload::WorkerPaneCapture`]
/// text snapshots of a worker's pane. Grammar: `pane_capture.<run_id>`.
/// Snapshots arrive only after the hub and executor negotiate pane-capture
/// and engine session wiring maps this topic onto host capture calls; that
/// wiring is separate from this protocol crate.
pub fn pane_capture_topic(run_id: &str) -> String {
    format!("{PANE_CAPTURE_TOPIC_PREFIX}{run_id}")
}

/// The run a [`pane_capture_topic`] names, or `None` for any other topic.
pub fn pane_capture_topic_run(topic: &str) -> Option<&str> {
    topic
        .strip_prefix(PANE_CAPTURE_TOPIC_PREFIX)
        .filter(|run| !run.is_empty())
}
