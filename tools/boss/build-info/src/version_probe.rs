//! Tiny binary whose only job is to print the leaf crate's stamped version.
//! Used by `//tools/boss/installer:build_info_stamp_test` so a stamped
//! `--version` path is covered without compiling engine_lib.

fn main() {
    println!("{}", boss_build_info::version_string("probe"));
}
