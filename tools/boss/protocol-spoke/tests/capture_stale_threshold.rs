//! Pins the macOS viewer's stale threshold to the spoke capture keep-alive.
//!
//! The app copies `CAPTURE_KEEP_ALIVE_SECONDS` and `DEFAULT_CAPTURE_INTERVAL_MS`
//! into Swift. A producer-only cadence change must fail here so a healthy idle
//! pane is not marked Stale between keep-alives.

use boss_protocol_spoke::limits::{CAPTURE_KEEP_ALIVE_SECONDS, DEFAULT_CAPTURE_INTERVAL_MS};

fn swift_source() -> String {
    let path = std::env::var("REMOTE_PANE_VIEWER_SWIFT")
        .expect("REMOTE_PANE_VIEWER_SWIFT names the live RemotePaneViewer.swift");
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {path}: {err}"))
}

fn swift_int(source: &str, name: &str) -> u32 {
    let needle = format!("static let {name} = ");
    let rest = source
        .split_once(&needle)
        .unwrap_or_else(|| panic!("RemotePaneViewer.swift is missing `{needle}`"))
        .1
        .trim_start();
    let token = rest
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| panic!("RemotePaneViewer.swift `{name}` is not an integer literal"));
    token
        .parse()
        .unwrap_or_else(|_| panic!("RemotePaneViewer.swift `{name}` is not a u32"))
}

#[test]
fn app_stale_threshold_tracks_spoke_keep_alive() {
    let swift = swift_source();
    let keep_alive = swift_int(&swift, "spokeKeepAliveSeconds");
    let interval = swift_int(&swift, "spokeCaptureIntervalSeconds");
    let slack = swift_int(&swift, "staleRenewalSlackSeconds");
    assert_eq!(
        keep_alive, CAPTURE_KEEP_ALIVE_SECONDS,
        "RemotePaneViewer.swift spokeKeepAliveSeconds must match CAPTURE_KEEP_ALIVE_SECONDS"
    );
    assert_eq!(
        interval.checked_mul(1000).expect("interval overflow"),
        DEFAULT_CAPTURE_INTERVAL_MS,
        "RemotePaneViewer.swift spokeCaptureIntervalSeconds must match DEFAULT_CAPTURE_INTERVAL_MS"
    );
    assert!(
        slack > 0,
        "staleRenewalSlackSeconds must be positive so a late keep-alive is not marked stale"
    );
    assert!(
        swift.contains(
            "staleAfterSeconds = spokeKeepAliveSeconds + spokeCaptureIntervalSeconds + staleRenewalSlackSeconds"
        ),
        "RemotePaneViewer.swift must derive staleAfterSeconds from keep-alive + interval + slack"
    );
    let threshold = keep_alive
        .checked_add(interval)
        .and_then(|sum| sum.checked_add(slack))
        .expect("stale threshold overflow");
    assert!(
        threshold > keep_alive + interval,
        "the viewer's stale threshold must outlast keep-alive plus one capture interval"
    );
}
