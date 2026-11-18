use super::*;
use tempfile::TempDir;

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A destination folder and a staging folder outside it.
fn dirs(tmp: &TempDir) -> (PathBuf, PathBuf) {
    let host = tmp.path().join("dest").join("host");
    std::fs::create_dir_all(&host).unwrap();
    (host, tmp.path().join("staging"))
}

#[test]
fn copies_content_and_leaves_no_staging_file() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("state.db.bak-20260101-120000");
    std::fs::write(&src, b"snapshot").unwrap();
    let (host, staging) = dirs(&tmp);
    let out = copy_to_offsite(&src, &host, &staging).unwrap();
    assert_eq!(out.copied_path, host.join("state.db.bak-20260101-120000"));
    assert_eq!(out.bytes, 8);
    assert_eq!(std::fs::read(&out.copied_path).unwrap(), b"snapshot");
    assert_eq!(names(&host), ["state.db.bak-20260101-120000"]);
    assert!(names(&staging).is_empty(), "staging emptied: {:?}", names(&staging));
}

/// Yields its bytes in two reads and, between them, records what each
/// folder holds while the copy is half written.
struct ObservingReader<'a> {
    chunks: Vec<&'static [u8]>,
    host: &'a Path,
    staging: &'a Path,
    seen_in_host: Vec<String>,
    seen_in_staging: Vec<String>,
}

impl Read for ObservingReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.chunks.is_empty() {
            return Ok(0);
        }
        self.seen_in_host.extend(names(self.host));
        self.seen_in_staging.extend(names(self.staging));
        let chunk = self.chunks.remove(0);
        buf[..chunk.len()].copy_from_slice(chunk);
        Ok(chunk.len())
    }
}

#[test]
fn destination_sees_nothing_until_the_complete_file_appears() {
    let tmp = TempDir::new().unwrap();
    let (host, staging) = dirs(&tmp);
    let mut reader = ObservingReader {
        chunks: vec![b"first-", b"second"],
        host: &host,
        staging: &staging,
        seen_in_host: Vec::new(),
        seen_in_staging: Vec::new(),
    };
    let name = "state.db.bak-20260101-120000";
    let out = copy_stream_to_offsite(&mut reader, name, &host, &staging).unwrap();
    assert!(
        reader.seen_in_host.is_empty(),
        "nothing may appear in the destination mid-copy: {:?}",
        reader.seen_in_host
    );
    assert!(
        !reader.seen_in_staging.is_empty(),
        "the copy is staged outside the destination"
    );
    assert_eq!(out.bytes, 12);
    assert_eq!(std::fs::read(host.join(name)).unwrap(), b"first-second");
    assert_eq!(names(&host), [name]);
    assert!(names(&staging).is_empty());
}

#[test]
fn unrelated_partial_in_destination_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("state.db.bak-20260101-120000");
    std::fs::write(&src, b"good").unwrap();
    let (host, staging) = dirs(&tmp);
    std::fs::write(host.join(".state.db.bak-20260101-120000.partial"), b"half").unwrap();
    let out = copy_to_offsite(&src, &host, &staging).unwrap();
    assert_eq!(std::fs::read(out.copied_path).unwrap(), b"good");
    assert_eq!(
        std::fs::read(host.join(".state.db.bak-20260101-120000.partial")).unwrap(),
        b"half"
    );
}

#[test]
fn leftover_staged_file_is_replaced_not_published() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("state.db.bak-20260101-120000");
    std::fs::write(&src, b"good").unwrap();
    let (host, staging) = dirs(&tmp);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("state.db.bak-20260101-120000"), b"crashed-half").unwrap();
    let out = copy_to_offsite(&src, &host, &staging).unwrap();
    assert_eq!(std::fs::read(out.copied_path).unwrap(), b"good");
    assert!(names(&staging).is_empty());
}

#[test]
fn failure_leaves_no_final_file_and_cleans_staging() {
    struct FailingReader(bool);
    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                return Err(std::io::Error::other("source interrupted"));
            }
            buf[..4].copy_from_slice(b"half");
            Ok(4)
        }
    }
    let tmp = TempDir::new().unwrap();
    let (host, staging) = dirs(&tmp);
    let name = "state.db.bak-20260101-120000";
    assert!(copy_stream_to_offsite(&mut FailingReader(false), name, &host, &staging).is_err());
    assert!(names(&host).is_empty());
    assert!(names(&staging).is_empty());

    let missing_src = tmp.path().join(name);
    assert!(copy_to_offsite(&missing_src, &host, &staging).is_err());
    assert!(names(&host).is_empty());
}

#[test]
fn non_directory_destination_fails_and_cleans_staging() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("state.db.bak-20260101-120000");
    std::fs::write(&src, b"x").unwrap();
    let blocked = tmp.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let staging = tmp.path().join("staging");
    assert!(copy_to_offsite(&src, &blocked, &staging).is_err());
    assert!(names(&staging).is_empty());
}

const NAME: &str = "state.db.bak-20260101-120000";

fn route(tmp: &TempDir, relays: &[PathBuf], reader: &mut impl Read) -> (PathBuf, PathBuf, Result<CopyOutcome>) {
    let (host, staging) = dirs(tmp);
    let out = copy_via_route(reader, NAME, &host, &staging, false, relays);
    (host, staging, out)
}

#[test]
fn cross_volume_route_relays_outside_the_destination() {
    let tmp = TempDir::new().unwrap();
    let relay = tmp.path().join("relay");
    let (host, staging) = dirs(&tmp);
    let mut reader = ObservingReader {
        chunks: vec![b"first-", b"second"],
        host: &host,
        staging: &staging,
        seen_in_host: Vec::new(),
        seen_in_staging: Vec::new(),
    };
    let out = copy_via_route(&mut reader, NAME, &host, &staging, false, std::slice::from_ref(&relay)).unwrap();
    assert!(reader.seen_in_host.is_empty(), "{:?}", reader.seen_in_host);
    assert_eq!(std::fs::read(&out.copied_path).unwrap(), b"first-second");
    assert_eq!(names(&host), [NAME]);
    assert!(names(&staging).is_empty());
    assert!(names(&relay).is_empty());
}

#[test]
fn cross_volume_without_relay_leaves_destination_empty() {
    let tmp = TempDir::new().unwrap();
    let (host, staging, out) = route(&tmp, &[], &mut &b"payload"[..]);
    assert!(out.unwrap_err().to_string().contains("copy"));
    assert!(names(&host).is_empty());
    assert!(names(&staging).is_empty());
}

#[test]
fn publication_observed_during_copy_retries_both_cross_device_renames() {
    let tmp = TempDir::new().unwrap();
    let (host, staging) = dirs(&tmp);
    std::fs::create_dir_all(&staging).unwrap();
    let relays = [tmp.path().join("rejected"), tmp.path().join("accepted")];
    let mut attempts = 0;
    let mut copies = 0;
    let mut rename = |from: &Path, to: &Path| {
        attempts += 1;
        assert!(names(&host).is_empty());
        assert_eq!(std::fs::read(from).unwrap(), b"first-second");
        if attempts <= 2 {
            return Err(std::io::Error::from_raw_os_error(18));
        }
        std::fs::rename(from, to)?;
        assert_eq!(names(&host), [NAME]);
        assert_eq!(std::fs::read(to).unwrap(), b"first-second");
        Ok(())
    };
    let mut relay_copy = |from: &Path, to: &Path| {
        copies += 1;
        assert_eq!(std::fs::read(from).unwrap(), b"first-second");
        let mut reader = ObservingReader {
            chunks: vec![b"first-", b"second"],
            host: &host,
            staging: to.parent().unwrap(),
            seen_in_host: Vec::new(),
            seen_in_staging: Vec::new(),
        };
        let bytes = write_stream_atomic(to, &mut reader)?;
        assert!(reader.seen_in_host.is_empty());
        assert!(!reader.seen_in_staging.is_empty());
        Ok(bytes)
    };
    let bytes = publish_with_ops(
        &mut &b"first-second"[..],
        &staging.join(NAME),
        &host.join(NAME),
        true,
        &relays,
        &mut rename,
        &mut relay_copy,
    )
    .unwrap();
    assert_eq!(bytes, 12);
    assert_eq!(attempts, 3);
    assert_eq!(copies, 2);
    verify_published(&host.join(NAME), bytes).unwrap();
    assert!(names(&staging).is_empty());
    for relay in relays {
        assert!(names(&relay).is_empty());
    }
}

#[test]
fn publication_failure_cleans_local_and_relay_files() {
    for fail_copy in [true, false] {
        let tmp = TempDir::new().unwrap();
        let (host, staging) = dirs(&tmp);
        std::fs::create_dir_all(&staging).unwrap();
        let relay = tmp.path().join("relay");
        let mut relay_copy = |from: &Path, to: &Path| {
            if fail_copy {
                struct Interrupted<'a>(&'a Path, bool);
                impl Read for Interrupted<'_> {
                    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                        assert!(names(self.0).is_empty());
                        if std::mem::replace(&mut self.1, true) {
                            return Err(std::io::Error::other("relay interrupted"));
                        }
                        buf[..4].copy_from_slice(b"half");
                        Ok(4)
                    }
                }
                write_stream_atomic(to, &mut Interrupted(&host, false))
            } else {
                write_stream_atomic(to, &mut File::open(from)?)
            }
        };
        let mut rename = |_: &Path, _: &Path| {
            assert!(names(&host).is_empty());
            Err(std::io::Error::other("publication denied"))
        };
        assert!(
            publish_with_ops(
                &mut &b"snapshot"[..],
                &staging.join(NAME),
                &host.join(NAME),
                false,
                std::slice::from_ref(&relay),
                &mut rename,
                &mut relay_copy,
            )
            .is_err()
        );
        assert!(names(&host).is_empty());
        assert!(names(&staging).is_empty());
        assert!(names(&relay).is_empty());
    }
}

#[test]
fn all_relays_rejected_never_write_inside_destination() {
    let tmp = TempDir::new().unwrap();
    let (host, staging) = dirs(&tmp);
    std::fs::create_dir_all(&staging).unwrap();
    let relay = tmp.path().join("relay");
    let mut rename = |_: &Path, _: &Path| {
        assert!(names(&host).is_empty());
        Err(std::io::Error::from_raw_os_error(18))
    };
    let mut relay_copy = |from: &Path, to: &Path| {
        assert!(names(&host).is_empty());
        assert_eq!(std::fs::read(from)?, b"snapshot");
        let mut reader = ObservingReader {
            chunks: vec![b"snap", b"shot"],
            host: &host,
            staging: to.parent().unwrap(),
            seen_in_host: Vec::new(),
            seen_in_staging: Vec::new(),
        };
        let bytes = write_stream_atomic(to, &mut reader)?;
        assert!(reader.seen_in_host.is_empty());
        assert!(!reader.seen_in_staging.is_empty());
        Ok(bytes)
    };
    assert!(
        publish_with_ops(
            &mut &b"snapshot"[..],
            &staging.join(NAME),
            &host.join(NAME),
            true,
            std::slice::from_ref(&relay),
            &mut rename,
            &mut relay_copy,
        )
        .is_err()
    );
    assert!(names(&host).is_empty());
    assert!(names(&staging).is_empty());
    assert!(names(&relay).is_empty());
}

#[test]
fn cross_volume_failure_leaves_nothing_behind() {
    struct Failing(bool);
    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                return Err(std::io::Error::other("source interrupted"));
            }
            buf[..4].copy_from_slice(b"half");
            Ok(4)
        }
    }
    let tmp = TempDir::new().unwrap();
    let relay = tmp.path().join("relay");
    std::fs::create_dir_all(&relay).unwrap();
    let (host, staging, out) = route(&tmp, std::slice::from_ref(&relay), &mut Failing(false));
    assert!(out.is_err());
    assert!(names(&host).is_empty());
    assert!(names(&staging).is_empty());
    assert!(names(&relay).is_empty());
}

#[test]
fn cross_device_rename_errors_are_recognised() {
    assert!(is_cross_device(&std::io::Error::from_raw_os_error(18)));
    assert!(is_cross_device(&std::io::Error::from(
        std::io::ErrorKind::CrossesDevices
    )));
    assert!(!is_cross_device(&std::io::Error::other("nope")));
}

#[test]
fn destination_volume_candidate_is_beside_destination_root() {
    let tmp = TempDir::new().unwrap();
    let (host, _) = dirs(&tmp);
    assert!(relay_candidates(&host).contains(&tmp.path().join(".boss-offsite-relay")));
}

#[cfg(unix)]
#[test]
fn relay_symlink_into_destination_is_rejected_before_sweeping() {
    let tmp = TempDir::new().unwrap();
    let (host, _) = dirs(&tmp);
    let relay = tmp.path().join(".boss-offsite-relay");
    std::os::unix::fs::symlink(&host, &relay).unwrap();
    assert!(!relay_candidates(&host).contains(&relay));
}

#[test]
fn same_volume_is_detected_and_unknown_means_different() {
    let tmp = TempDir::new().unwrap();
    let (host, staging) = dirs(&tmp);
    std::fs::create_dir_all(&staging).unwrap();
    assert!(on_same_volume(&staging, &host));
    assert!(!on_same_volume(&staging, &tmp.path().join("unmounted")));
}

#[test]
fn incomplete_published_copy_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let published = tmp.path().join("state.db.bak-20260101-120000");
    std::fs::write(&published, b"short").unwrap();
    assert!(verify_published(&published, 5).is_ok());
    let err = verify_published(&published, 9).unwrap_err();
    assert!(err.to_string().contains("incomplete"), "{err}");
    assert!(verify_published(&tmp.path().join("absent"), 5).is_err());
    assert!(verify_published(tmp.path(), 5).is_err());
}

#[test]
fn refuses_live_database_files() {
    let tmp = TempDir::new().unwrap();
    for name in ["state.db", "state.db-wal", "state.db-shm"] {
        let src = tmp.path().join(name);
        std::fs::write(&src, b"live").unwrap();
        let err = copy_to_offsite(&src, tmp.path(), &tmp.path().join("staging")).unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");
    }
}
