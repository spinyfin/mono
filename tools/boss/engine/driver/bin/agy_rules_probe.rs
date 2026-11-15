fn main() -> anyhow::Result<()> {
    let directory = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: agy_rules_probe <new absolute evidence directory>; BOSS_AGY_BIN selects the host CLI")
    })?;
    let directory = std::path::PathBuf::from(directory);
    anyhow::ensure!(directory.is_absolute(), "evidence directory must be absolute");
    std::fs::create_dir(&directory)?;
    // This standalone process has no threads yet; never mutate engine globals.
    unsafe {
        std::env::set_var("BOSS_AGY_HOMES_DIR", directory.join("homes"));
        std::env::remove_var("BOSS_AGY_SKIP_BINARY_ASSERT");
        std::env::remove_var("BOSS_AGY_SKIP_POSTURE_ASSERT");
    }
    boss_engine_driver::agy::rules_probe::run(&directory)
}
