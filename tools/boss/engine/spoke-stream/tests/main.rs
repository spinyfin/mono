mod commands;
mod store;
mod support;
mod transport;

use boss_protocol_spoke::v1::*;
fn run() -> Run {
    Run {
        execution_id: "execution".into(),
        run_id: "run".into(),
        assignment_generation: 1,
        spawn_token: "token".into(),
    }
}
fn record(sequence: u64) -> Record {
    Record {
        sequence,
        observed: Some(Stamp {
            boot_id: "boot".into(),
            ..Default::default()
        }),
        run: Some(run()),
        body: Some(record::Body::Pane(PaneState {
            life: PaneLife::Alive.into(),
            boot_id: "boot".into(),
            ..Default::default()
        })),
    }
}
fn command(id: &str) -> Command {
    Command {
        command_id: id.into(),
        run: Some(run()),
        not_after_unix_ms: 4_102_444_800_000,
        body: Some(command::Body::Stop(Stop { grace_ms: 1 })),
        ..Default::default()
    }
}
