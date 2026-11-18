//! A revoke committed by another handle while token minting is in flight holds.
use boss_host_enrollment::{Clock, CredentialError, Denial, HostCredentials, HostState, Policy};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};
use std::time::SystemTime;

struct PausedClock {
    armed: AtomicBool,
    entered: Barrier,
    resume: Barrier,
}
impl Clock for PausedClock {
    fn now(&self) -> SystemTime {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.wait();
            self.resume.wait();
        }
        SystemTime::now()
    }
}

#[test]
fn another_handle_revokes_during_manual_mint() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let clock = Arc::new(PausedClock {
        armed: AtomicBool::new(false),
        entered: Barrier::new(2),
        resume: Barrier::new(2),
    });
    let issuer = HostCredentials::open(dir.path(), Policy::default(), clock.clone()).unwrap();
    issuer.authorize_host("edge", "operator").unwrap();
    let revoker = HostCredentials::open_existing(
        dir.path(),
        Policy::default(),
        Arc::new(boss_host_enrollment::SystemClock),
    )
    .unwrap();
    clock.armed.store(true, Ordering::SeqCst);
    let mint = std::thread::spawn(move || issuer.issue_manual_token("edge", "operator", false));
    clock.entered.wait();
    revoker.revoke("edge", "operator", "removed").unwrap();
    clock.resume.wait();
    assert!(matches!(
        mint.join().unwrap(),
        Err(CredentialError::Denied(Denial::Revoked))
    ));
    assert_eq!(revoker.status("edge").unwrap().unwrap().state, HostState::Revoked);
    assert!(
        !revoker
            .audit_trail("edge")
            .unwrap()
            .iter()
            .any(|(event, _)| event == "reauthorized")
    );
}
