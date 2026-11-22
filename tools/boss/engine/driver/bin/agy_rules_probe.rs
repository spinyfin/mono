const USAGE: &str = "usage: agy_rules_probe <new absolute evidence directory> [--scratch]; \
                     BOSS_AGY_BIN selects the host CLI. --scratch measures a run with no cube \
                     checkout: rules at the working directory's root, no --add-dir";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let kind = match args.next() {
        None => boss_engine_driver::ExecutionWorkspaceKind::Cube,
        Some(flag) if flag == "--scratch" => boss_engine_driver::ExecutionWorkspaceKind::Scratch,
        Some(_) => anyhow::bail!(USAGE),
    };
    let directory = std::path::PathBuf::from(directory);
    // SAFETY: this binary has not started any threads.
    unsafe {
        boss_engine_driver::agy::probe_isolation::prepare_evidence_dir(&directory)?;
    }
    boss_engine_driver::agy::rules_probe::run(&directory, kind)
}
