fn main() -> anyhow::Result<()> {
    let directory = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: agy_rules_probe <new absolute evidence directory>; BOSS_AGY_BIN selects the host CLI")
    })?;
    let directory = std::path::PathBuf::from(directory);
    boss_engine_driver::agy::probe_isolation::prepare_evidence_dir(&directory)?;
    boss_engine_driver::agy::rules_probe::run(&directory)
}
