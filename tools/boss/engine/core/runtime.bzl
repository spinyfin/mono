"""Residual runtime libraries and their shared fixture dependency substitutions."""

load("@mono_crates//:defs.bzl", "all_crate_deps")
load("@rules_rust//rust:defs.bzl", "rust_library")
load(":codex_cli_version.bzl", "CODEX_CLI_VERSION")
load(":engine_binary.bzl", "ENGINE_BINARY_BAZEL_RUN_COMMAND", "ENGINE_BINARY_TARGET")

TEST_ALIASES = {
    "//tools/boss/engine/completion": "boss_engine_completion_production",
    "//tools/boss/engine/completion:completion_test_support": "boss_engine_completion",
    "//tools/boss/engine/dispatch": "boss_engine_dispatch_production",
    "//tools/boss/engine/dispatch:dispatch_test_support": "boss_engine_dispatch",
    "//tools/boss/engine/execution-runner": "boss_execution_runner_production",
    "//tools/boss/engine/execution-runner:runner_test_support": "boss_execution_runner",
    "//tools/boss/engine/execution-contracts": "boss_execution_contracts_production",
    "//tools/boss/engine/execution-contracts:execution_contracts_test_support": "boss_execution_contracts",
    "//tools/boss/engine/live-worker-state": "boss_engine_live_worker_state_production",
    "//tools/boss/engine/live-worker-state:live_worker_state_test_support": "boss_engine_live_worker_state",
    "//tools/boss/engine/merge-poller": "boss_engine_merge_poller_production",
    "//tools/boss/engine/merge-poller:merge_poller_test_support": "boss_engine_merge_poller",
    "//tools/boss/engine/observation-contracts": "boss_observation_contracts_production",
    "//tools/boss/engine/observation-contracts:observation_contracts_test_support": "boss_observation_contracts",
    "//tools/boss/engine/work-db:work-db": "boss_work_db_production",
    "//tools/boss/engine/work-db:work_db_test_support": "boss_work_db",
}

TEST_SWAPS = [
    "//tools/boss/engine/completion",
    "//tools/boss/engine/dispatch",
    "//tools/boss/engine/execution-contracts",
    "//tools/boss/engine/execution-runner",
    "//tools/boss/engine/live-worker-state",
    "//tools/boss/engine/merge-poller",
    "//tools/boss/engine/observation-contracts",
    "//tools/boss/engine/work-db",
]

TEST_DEPS = [
    "//tools/boss/engine/completion:completion_test_support",
    "//tools/boss/engine/dispatch:dispatch_test_support",
    "//tools/boss/engine/execution-contracts:execution_contracts_test_support",
    "//tools/boss/engine/execution-runner:runner_test_support",
    "//tools/boss/engine/live-worker-state:live_worker_state_test_support",
    "//tools/boss/engine/merge-poller:merge_poller_test_support",
    "//tools/boss/engine/observation-contracts:observation_contracts_test_support",
    "//tools/boss/engine/work-db:work_db_test_support",
]

def runtime_libraries():
    """Declare production runtime and its test-only fixture variant in core."""
    attrs = dict(
        srcs = native.glob(
            ["src/**/*.rs"],
            # Keep the library in lockstep with the module tree: a hand-maintained
            # explicit list silently drifts when a new `mod` file is added (it is
            # then absent from rustc's inputs -> E0583). Every other boss crate globs.
            exclude = [
                # Crate root of the `engine` rust_binary in `core/BUILD.bazel`, not the library.
                "src/main.rs",
                "src/facade.rs",
            ],
        ),
        compile_data = [
            "remote/boss-remote-run.sh",
            # Stamped build-info constants (BOSS_VERSION, BOSS_GIT_SHA, BOSS_BUILD_TIME).
            # Referenced via include!(env!("BOSS_BUILD_INFO_RS")) in build_info_stamp module.
            "//tools/boss/installer:build_info_rs",
            # Read by boss_construct_scan.rs's self-test to assert the CHECKS.yaml
            # boss-ism pattern stays byte-identical to BOSS_ID_RE.
            "//:CHECKS.yaml",
            # Reference-driver conformance goldens — include_str!'d from
            # src/conformance/claude_goldens.rs.
            "src/conformance/goldens/settings_standard.json.golden",
            "src/conformance/goldens/claude_md_standard.md.golden",
            # Planner parser regressions read these live docs directly so their
            # expectations cannot drift from the source of truth.
            "//tools/boss/docs:operator_forced_dispatch_design_doc",
            "//tools/boss/docs:engine_app_rpc_design_doc",
        ],
        crate_name = "boss_engine_runtime",
        crate_root = "src/lib.rs",
        edition = "2024",
        proc_macro_deps = all_crate_deps(package_name = "tools/boss/engine/core/runtime", proc_macro = True),
        rustc_env = {
            # Absolute path to the generated build-info constants file.
            # rules_rust prefixes $(execpath) values with ${pwd} and resolves
            # them to absolute filesystem paths before invoking rustc.
            "BOSS_BUILD_INFO_RS": "$(execpath //tools/boss/installer:build_info_rs)",
            # Absolute path to the root CHECKS.yaml, read by
            # boss_construct_scan.rs's self-test via include_str!(env!(...)). A
            # plain relative include_str! can't cross this package boundary: the
            # source file's compile-time path is rooted under bazel-out, not the
            # real source tree, so `../../../CHECKS.yaml` doesn't resolve there.
            "BOSS_CHECKS_YAML": "$(execpath //:CHECKS.yaml)",
            # Absolute path to the real operator-forced-dispatch design doc, read
            # by the planner breakdown-authority regression test via
            # include_str!(env!(...)) — same cross-package-boundary reason as
            # BOSS_CHECKS_YAML above. Keeps the test reading the source of truth
            # instead of a testdata copy that can silently drift.
            "BOSS_OPERATOR_FORCED_DISPATCH_DESIGN_DOC": "$(execpath //tools/boss/docs:operator_forced_dispatch_design_doc)",
            # Absolute path to a live design doc with bold-bullet breakdown
            # entries, read by the planner parser regression test.
            "BOSS_ENGINE_APP_RPC_DESIGN_DOC": "$(execpath //tools/boss/docs:engine_app_rpc_design_doc)",
            # Canonical `bazel run` command for the engine target, so worker-facing
            # guidance (worker_setup.rs, agent_launch_guard.rs) can't drift from
            # the real target label the way the pre-split `//tools/boss/engine:engine`
            # string did.
            "BOSS_ENGINE_BAZEL_RUN_COMMAND": ENGINE_BINARY_BAZEL_RUN_COMMAND,
            "BOSS_ENGINE_BAZEL_TARGET": ENGINE_BINARY_TARGET,
            # Same source of truth as the codex_test_* http_archive stanzas in
            # MODULE.bazel and the driver's config-compatibility test, so
            # PINNED_CODEX_CLI_VERSION can never drift from the release the
            # driver's test binaries are fetched from.
            "CODEX_CLI_VERSION": CODEX_CLI_VERSION,
        },
        visibility = ["//tools/boss/engine/app:__pkg__"],
        deps = [
            "//tools/boss/engine/execution-runner",
            "//tools/boss/engine/host-execution",
            "//tools/boss/engine/execution-contracts",
            "//tools/boss/engine/observation-contracts",
            "//lib/rust/git_utils",
            # NOT "//tools/boss/build-provenance": that crate's source embeds
            # the commit sha and changes every commit. It is linked only by the
            # two `engine` rust_binary targets in `core/BUILD.bazel`, which hand the values to
            # `build_info::install_provenance` at startup. See
            # `stamped_provenance_isolation_test` in `core/BUILD.bazel`.
            "//tools/boss/claude_client",
            "//tools/boss/codex_auth",
            "//tools/boss/command-runner",
            "//tools/boss/comment_anchor",
            "//tools/boss/engine/attachments",
            "//tools/boss/engine/automation-schedule",
            "//tools/boss/engine/ci-log-reader",
            "//tools/boss/engine/codex-rollout-retention",
            "//tools/boss/engine/comment-classifier",
            "//tools/boss/engine/completion",
            "//tools/boss/engine/conflict-diagnosis",
            "//tools/boss/engine/day-rotated-log",
            "//tools/boss/engine/design-doc-fetcher",
            "//tools/boss/engine/design-docs",
            "//tools/boss/engine/deterministic-resolvers",
            "//tools/boss/engine/dispatch",
            "//tools/boss/engine/dispatch-events",
            "//tools/boss/engine/dispatch-reader",
            "//tools/boss/engine/driver",
            "//tools/boss/engine/driver-quota",
            "//tools/boss/engine/editorial",
            "//tools/boss/engine/effort",
            "//tools/boss/engine/event-bus",
            "//tools/boss/engine/feature-flags",
            "//tools/boss/engine/gh-invocation",
            "//tools/boss/engine/live-worker-state",
            "//tools/boss/engine/grok-home-retention",
            "//tools/boss/engine/live-status-redact",
            "//tools/boss/engine/merge-poller",
            "//tools/boss/engine/metrics",
            "//tools/boss/engine/offsite-backup",
            "//tools/boss/engine/planner-validation",
            "//tools/boss/engine/pr-review",
            "//tools/boss/engine/pr-review-sources",
            "//tools/boss/engine/pr-template",
            "//tools/boss/engine/recovery",
            "//tools/boss/engine/review-guide",
            "//tools/boss/engine/ssh-transport",
            "//tools/boss/engine/stdout-progress",
            "//tools/boss/engine/storage-values",
            "//tools/boss/engine/structured-output",
            "//tools/boss/engine/task-scope",
            "//tools/boss/engine/timer-wheel",
            "//tools/boss/engine/transcript-markdown:transcript_markdown",
            "//tools/boss/engine/transcript-tail",
            "//tools/boss/engine/transient-error",
            "//tools/boss/engine/utility-model",
            "//tools/boss/engine/utils",
            "//tools/boss/engine/worker-bin",
            "//tools/boss/engine/work-db",
            "//tools/boss/gh_telemetry",
            "//tools/boss/github",
            "//tools/boss/github_tracker",
            "//tools/boss/log-files",
            "//tools/boss/protocol",
            "//tools/boss/tmux",
        ] + all_crate_deps(package_name = "tools/boss/engine/core/runtime", normal = True),
    )
    rust_library(name = "runtime", **attrs)
    fixture_attrs = dict(attrs)
    fixture_attrs.update(
        testonly = True,
        aliases = {k: v for k, v in TEST_ALIASES.items() if not v.endswith("_production")},
        crate_features = ["test-support"],
        crate_name = "boss_engine_runtime_test_support",
        deps = [d for d in attrs["deps"] if d not in TEST_SWAPS] + TEST_DEPS + [
            "@mono_crates//:tempfile",
        ],
    )
    rust_library(name = "runtime_test_support", **fixture_attrs)
