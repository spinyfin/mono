fn main() -> anyhow::Result<()> {
    let directory = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: scoped_bazel_probe <absolute evidence directory>"))?;
    boss_engine_driver::scoped_home::bazel_probe::run(std::path::Path::new(&directory))
}
