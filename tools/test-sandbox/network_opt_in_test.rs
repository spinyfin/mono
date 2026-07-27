use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

#[test]
fn target_level_network_opt_in_removes_the_external_network_boundary() {
    let address: SocketAddr = "1.1.1.1:443".parse().unwrap();
    if let Err(error) = TcpStream::connect_timeout(&address, Duration::from_secs(1)) {
        assert!(
            !matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::NetworkUnreachable
            ),
            "the target-level network opt-in did not reach the platform sandbox: {error}"
        );
    }
}
