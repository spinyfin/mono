//! Darwin process arguments without launching the setuid `ps` executable,
//! which macOS refuses to execute inside a Seatbelt sandbox.

use std::ffi::{c_int, c_void};

unsafe extern "C" {
    fn sysctl(
        name: *mut c_int,
        namelen: u32,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
}

fn read_sysctl(name: &mut [c_int], buffer: &mut [u8]) -> Option<usize> {
    let mut len = buffer.len();
    // SAFETY: Both slices are valid for their supplied lengths, len is a
    // writable size_t, and the null newp requests a read-only operation.
    let result = unsafe {
        sysctl(
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

pub(super) fn command(pid: u32) -> Option<String> {
    // CTL_KERN, KERN_ARGMAX and KERN_PROCARGS2 from Darwin's sys/sysctl.h.
    let mut size = [0; size_of::<c_int>()];
    read_sysctl(&mut [1, 8], &mut size)?;
    let capacity = usize::try_from(c_int::from_ne_bytes(size)).ok()?;
    let mut buffer = vec![0; capacity];
    let len = read_sysctl(&mut [1, 49, pid.try_into().ok()?], &mut buffer)?;
    parse_command(&buffer[..len])
}

fn parse_command(buffer: &[u8]) -> Option<String> {
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
        command.push(String::from_utf8_lossy(arg));
    }
    Some(command.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_excludes_environment_and_preserves_arguments() {
        let mut buffer = 3_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/sh\0\0\0/bin/sh\0-c\0engine argument\0ENV=ignored\0");
        assert_eq!(parse_command(&buffer).as_deref(), Some("/bin/sh -c engine argument"));
        buffer.truncate(8);
        assert!(parse_command(&buffer).is_none());
        assert!(parse_command(&[]).is_none());
    }
}
