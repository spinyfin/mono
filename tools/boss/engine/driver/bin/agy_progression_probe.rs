const USAGE: &str = "usage: agy_progression_probe <new absolute evidence directory>; \
                     BOSS_AGY_BIN selects the host CLI. Runs one short authenticated session \
                     under the production progression posture and reports which Boss verbs ran";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    anyhow::ensure!(args.next().is_none(), USAGE);
    let directory = std::path::PathBuf::from(directory);
    // SAFETY: this binary has not started any threads.
    unsafe {
        boss_engine_driver::agy::probe_isolation::prepare_evidence_dir(&directory)?;
    }
    boss_engine_driver::agy::progression_probe::run(&directory)
}
