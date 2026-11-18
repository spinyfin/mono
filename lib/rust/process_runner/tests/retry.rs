use process_runner::retry_text_file_busy;
use std::io;

#[test]
fn retries_busy_until_success() {
    let mut calls = 0;
    let result = retry_text_file_busy(|| {
        calls += 1;
        if calls < 3 {
            Err(io::Error::from_raw_os_error(libc::ETXTBSY))
        } else {
            Ok(42)
        }
    });
    assert_eq!(result.unwrap(), 42);
    assert_eq!(calls, 3);
}

#[test]
fn returns_other_errors_without_retrying() {
    let mut calls = 0;
    let result = retry_text_file_busy::<()>(|| {
        calls += 1;
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    });
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOENT));
    assert_eq!(calls, 1);
}

#[test]
fn persistent_busy_stops_after_fifty_retries() {
    let mut calls = 0;
    let result = retry_text_file_busy::<()>(|| {
        calls += 1;
        Err(io::Error::from_raw_os_error(libc::ETXTBSY))
    });
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ETXTBSY));
    assert_eq!(calls, 51);
}
