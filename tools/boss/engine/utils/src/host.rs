//! Host identity helpers.

/// This machine's hostname (`gethostname(2)`), or `unknown-host`.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid writable buffer of the stated length.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return "unknown-host".to_owned();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_name_is_non_empty() {
        assert!(!host_name().is_empty());
    }
}
