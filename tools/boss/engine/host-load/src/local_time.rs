//! Local-time rendering for the CLI. Storage is always UTC epoch
//! milliseconds; only display converts.

/// `2026-10-09 14:03:07 -0700` for an epoch-millisecond timestamp, in the
/// process's local timezone. Falls back to the raw milliseconds if the C
/// library can't convert it.
pub fn format_local_ms(ts_ms: i64) -> String {
    let secs: libc::time_t = ts_ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm` is plain integers/pointers; zeroed is a valid
    // placeholder that `localtime_r` fully overwrites on success.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the duration of the call.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return format!("{ts_ms}ms");
    }
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is valid for `buf.len()` bytes; the format is a
    // NUL-terminated literal.
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr().cast(),
            buf.len(),
            c"%Y-%m-%d %H:%M:%S %z".as_ptr(),
            &tm,
        )
    };
    if n == 0 {
        return format!("{ts_ms}ms");
    }
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_date_time_and_numeric_offset() {
        let s = format_local_ms(1_700_000_000_000);
        // Timezone-independent shape: `YYYY-MM-DD HH:MM:SS +ZZZZ`.
        let bytes = s.as_bytes();
        assert_eq!(s.len(), 25, "unexpected rendering {s:?}");
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[10], b' ');
        assert_eq!(bytes[13], b':');
        assert!(bytes[20] == b'+' || bytes[20] == b'-', "offset sign in {s:?}");
    }
}
