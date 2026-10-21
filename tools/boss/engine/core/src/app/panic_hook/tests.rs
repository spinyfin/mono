use super::*;
use crate::live_worker_state::LiveSpawnRouting;

#[test]
fn emergency_cleanup_requires_proven_local_child_ownership_and_non_tmux_hosting() {
    let (server, _temp) = super::super::tests::test_server_state();
    let mut child = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
    let pid = child.id() as i32;
    for (slot, hosting, host) in [
        (1, Some(true), Some("local")),
        (2, None, Some("local")),
        (3, Some(false), Some("remote")),
        (4, Some(false), None),
        (5, Some(false), Some("local")),
    ] {
        server.live_worker_states.register_spawn_with_capabilities(
            slot,
            format!("panic-test-{slot}"),
            "test-model",
            pid,
            None,
            false,
            LiveSpawnRouting {
                tmux_hosted: hosting,
                metadata: boss_protocol::LiveWorkerMetadata {
                    host_id: host.map(str::to_owned),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
    }
    let selected = emergency_shell_pids(&server);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(selected, vec![pid], "only the known non-tmux local child is eligible");
    assert!(
        emergency_shell_pids(&server).is_empty(),
        "a stale recorded PID proves no ownership"
    );
}
