fn main() -> anyhow::Result<()> {
    let directory = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: agy_quota_probe <new absolute evidence directory>; BOSS_AGY_BIN selects the host CLI")
    })?;
    boss_engine_driver::agy::probe_isolation::prepare_evidence_dir(std::path::Path::new(&directory))?;
    println!("{}", boss_engine_driver::agy::quota::usage_json()?);
    Ok(())
}
