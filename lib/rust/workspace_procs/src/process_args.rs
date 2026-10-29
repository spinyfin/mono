//! Process arguments without launching `ps`. Darwin uses sysctl because macOS
//! refuses to execute the setuid `ps` inside a Seatbelt sandbox; Linux uses procfs.

#[cfg(target_os = "macos")]
use std::ffi::c_int;

#[cfg(target_os = "macos")]
fn read_sysctl(name: &mut [c_int], buffer: &mut [u8]) -> Option<usize> {
    let mut len = buffer.len();
    // SAFETY: Both slices are valid for their supplied lengths, len is a
    // writable size_t, and the null newp requests a read-only operation.
    let result = unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            name.len().try_into().ok()?,
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && len <= buffer.len()).then_some(len)
}

#[cfg(target_os = "macos")]
pub fn arguments(pid: u32) -> Option<Vec<String>> {
    let mut size = [0; size_of::<c_int>()];
    read_sysctl(&mut [libc::CTL_KERN, libc::KERN_ARGMAX], &mut size)?;
    let capacity = usize::try_from(c_int::from_ne_bytes(size)).ok()?;
    let mut buffer = vec![0; capacity];
    let len = read_sysctl(
        &mut [libc::CTL_KERN, libc::KERN_PROCARGS2, pid.try_into().ok()?],
        &mut buffer,
    )?;
    parse_arguments(&buffer[..len])
}

#[cfg(target_os = "macos")]
fn parse_arguments(buffer: &[u8]) -> Option<Vec<String>> {
    let (argc, rest) = buffer.split_at_checked(size_of::<c_int>())?;
    let argc = usize::try_from(c_int::from_ne_bytes(argc.try_into().ok()?)).ok()?;
    if argc == 0 {
        return None;
    }
    // The executable path precedes NUL padding and argc NUL-terminated
    // arguments; environment strings follow and must not affect recognition.
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let rest = &rest[path_end..];
    let args_start = rest.iter().position(|byte| *byte != 0)?;
    let mut args = rest[args_start..].split_inclusive(|byte| *byte == 0);
    let mut command = Vec::new();
    for _ in 0..argc {
        let arg = args.next()?.strip_suffix(&[0])?;
        command.push(String::from_utf8_lossy(arg).into_owned());
    }
    Some(command)
}

#[cfg(target_os = "linux")]
pub fn arguments(pid: u32) -> Option<Vec<String>> {
    let bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let command = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect::<Vec<_>>();
    (!command.is_empty()).then_some(command)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn arguments(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn command_excludes_environment_and_preserves_arguments() {
        let mut buffer = 3_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/sh\0\0\0/bin/sh\0-c\0engine argument\0ENV=ignored\0");
        assert_eq!(
            parse_arguments(&buffer).unwrap(),
            vec!["/bin/sh", "-c", "engine argument"]
        );
        buffer.truncate(8);
        assert!(parse_arguments(&buffer).is_none());
        assert!(parse_arguments(&[]).is_none());
    }
}

/// Joined arguments for command recognition and display.
pub fn command(pid: u32) -> Option<String> {
    arguments(pid).map(|args| args.join(" "))
}
