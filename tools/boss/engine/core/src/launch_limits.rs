//! Shared preflight accounting for local and SSH launch plans.
use anyhow::{Result, bail};

/// Include NULs and the argv/envp pointer tables in exec's byte accounting.
pub(crate) fn environment_bytes() -> usize {
    std::env::vars_os()
        .map(|(key, value)| key.len() + value.len() + 2 + std::mem::size_of::<usize>())
        .sum()
}

pub(crate) fn local_arg_max() -> Result<usize> {
    // sysconf has no pointer arguments and does not mutate process state.
    let limit = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
    anyhow::ensure!(limit > 0, "cannot determine local ARG_MAX");
    Ok(limit as usize)
}

/// Shell source is a conservative bound for literal argv and added env
/// strings. Budget it twice (source plus expansion), with a pointer per
/// byte as an upper bound for even a command of one-character arguments.
pub(crate) fn check(command_bytes: usize, environment_bytes: usize, arg_max: usize, driver: &str) -> Result<()> {
    let argv_bytes = command_bytes.saturating_mul(2 + std::mem::size_of::<usize>());
    let total = argv_bytes.saturating_add(environment_bytes).saturating_add(4096);
    if total > arg_max {
        bail!(
            "refusing to spawn {driver}: launch argv bound {argv_bytes} bytes + environment \
             {environment_bytes} bytes + 4096 launcher bytes = {total} bytes exceeds ARG_MAX {arg_max} bytes"
        );
    }
    Ok(())
}

/// Runs on the remote host, so both the limit and inherited environment
/// reflect the SSH login shell, without transferring any environment values.
pub(crate) const REMOTE_PROBE: &str = "python3 -c 'import os,struct; print(os.sysconf(\"SC_ARG_MAX\"), sum(len(k)+len(v)+2+struct.calcsize(\"P\") for k,v in os.environb.items()))'";

#[cfg(test)]
mod tests {
    #[test]
    fn environment_can_push_fitting_argv_over_the_limit() {
        super::check(1000, 1000, 20000, "fake").unwrap();
        let error = super::check(1000, 10000, 20000, "fake").unwrap_err();
        assert!(error.to_string().contains("environment 10000 bytes"));
        assert!(error.to_string().contains("ARG_MAX 20000 bytes"));
    }
}
