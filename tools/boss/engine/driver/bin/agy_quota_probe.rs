use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;

fn main() -> anyhow::Result<()> {
    let directory = std::env::args_os().nth(1).ok_or_else(|| {
        anyhow::anyhow!("usage: agy_quota_probe <new absolute evidence directory>; BOSS_AGY_BIN selects the host CLI")
    })?;
    let directory = std::path::PathBuf::from(directory);
    anyhow::ensure!(directory.is_absolute(), "evidence directory must be absolute");
    DirBuilder::new().mode(0o700).create(&directory)?;
    // This standalone process has no threads yet; never mutate engine globals.
    unsafe {
        std::env::set_var("BOSS_AGY_HOMES_DIR", directory.join("homes"));
        // Provisioning links the run's transcripts into the durable store;
        // keep the probe's inside its own evidence directory, never Boss's.
        std::env::set_var("BOSS_WORKER_TRANSCRIPTS_ROOT", directory.join("transcripts"));
        std::env::remove_var("BOSS_AGY_SKIP_BINARY_ASSERT");
        std::env::remove_var("BOSS_AGY_SKIP_POSTURE_ASSERT");
    }

    println!("{}", boss_engine_driver::agy::quota::usage_json()?);
    Ok(())
}
