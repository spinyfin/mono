use std::process::ExitCode;

fn main() -> ExitCode {
    // Intercept --version/-V before Cli::parse() so we print the
    // canonical version string. The tag-derived value lives in the
    // boss_build_info leaf crate so a new release tag only recompiles
    // the leaf and relinks this binary, leaving `bossctl_lib` cached.
    if boss_build_info::print_version_if_requested(&boss_build_info::version_string("bossctl")) {
        return ExitCode::SUCCESS;
    }

    bossctl::run()
}
