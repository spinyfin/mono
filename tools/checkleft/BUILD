load("@rules_rust//rust:defs.bzl", "rust_binary", "rust_library", "rust_test")
load("@crates//:defs.bzl", "all_crate_deps")

rust_library(
    name = "checkleft_lib",
    crate_name = "checkleft",
    srcs = glob(
        ["src/**/*.rs"],
        exclude = ["src/main.rs"],
    ),
    crate_root = "src/lib.rs",
    deps = all_crate_deps(normal = True),
    proc_macro_deps = all_crate_deps(proc_macro = True),
    visibility = ["//visibility:public"],
)

rust_binary(
    name = "checkleft",
    srcs = ["src/main.rs"],
    crate_root = "src/main.rs",
    deps = all_crate_deps(normal = True) + [
        ":checkleft_lib",
    ],
    proc_macro_deps = all_crate_deps(proc_macro = True),
    visibility = ["//visibility:public"],
)

rust_test(
    name = "checkleft_lib_test",
    crate = ":checkleft_lib",
    deps = all_crate_deps(normal_dev = True),
    proc_macro_deps = all_crate_deps(proc_macro_dev = True),
)

rust_test(
    name = "checkleft_bin_test",
    crate = ":checkleft",
    deps = all_crate_deps(normal_dev = True),
    proc_macro_deps = all_crate_deps(proc_macro_dev = True),
)
