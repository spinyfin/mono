# Public policy test relocation evidence

## Unchanged test manifest

Compared against base db639c05. SHA-256 covers the entire function body after stripping line indentation and surrounding blank lines; all ten bodies match, including assertions and fixtures. Fully qualified test names are preserved.

| Test name                                                                        | Assertions | Body SHA-256                                                       |
| -------------------------------------------------------------------------------- | ---------: | ------------------------------------------------------------------ |
| cost_pricing::tests::matches_family_substring_case_insensitively                 |          3 | `6a36fbd904115341aebcd64a8763b5362a670f5fe99d9de014d7b7c740ad0895` |
| cost_pricing::tests::unknown_model_has_no_price                                  |          2 | `3ca40ffb23e9ada5d7fcfc654da11df4e2be18351fb13844b8a2cd2637831a60` |
| cost_pricing::tests::estimate_sums_every_token_category_at_its_own_rate          |          1 | `c1e880c8c40a843314c93dcd110e4cbba28626cb0fed36b637b40424c265cead` |
| cost_pricing::tests::zero_tokens_estimate_to_zero                                |          1 | `e3cc0b0fc892077310a76b26de0390dd93bd51d84511b509b2841391409925c2` |
| merge_mechanism::tests::none_and_direct_parse_to_direct                          |          2 | `4a0a6a01ad0db7aeca268cf818512627c907b1d3bb5b3a48e0770e88f62fb36c` |
| merge_mechanism::tests::trunk_queue_parses_with_default_target_branch            |          1 | `f8a551066e91726f48dc15f86a8084fde0bf1b935935c89e4be064fca2cddea8` |
| merge_mechanism::tests::unknown_value_is_rejected                                |          1 | `7b28a60e9ca1f5d2a80dde71a7d2046d8e7c885afeb03b8fa8b32f0141214cc6` |
| merge_mechanism::tests::push_restriction_markers_are_detected_case_insensitively |          6 | `ec1019188758fe0dcdf7e2675065c862414526a53e8815c6222a029d0fbd8dba` |
| merge_mechanism::tests::ordinary_merge_failures_are_not_push_restrictions        |          3 | `a4d7c3388571b0d7049619b6d32115926043469f724df4c97798693837fc2d9a` |
| merge_mechanism::tests::attention_body_names_the_fix                             |          4 | `221c15b3996c9215b958c1b85c721ac72e02dcfe1a97c0c6557bd207f06288a3` |

## Test-only edit profile

Observed on macOS arm64 with Bazel 9.1.0. After building both targets, added only the module-path comment in `tests/policy_contract.rs`, then ran:

```sh
bazel build //tools/boss/engine/core:policy_contract_test //tools/boss/engine/core:engine_lib_test --profile=/absolute/path/profile.json.gz --execution_log_json_file=/absolute/path/actions.json
```

Invocation: `3ad486b2-b7e0-472a-bf31-54a2d8fe87ab`, exit 0. Normal caches remained enabled. The execution log contained exactly these executed spawns:

| Mnemonic    | Target                                        | Runner         | Cache hit |
| ----------- | --------------------------------------------- | -------------- | --------- |
| Rustc       | //tools/boss/engine/core:policy_contract_test | darwin-sandbox | false     |
| BuildInfoRs | //tools/boss/installer:build_info_rs          | darwin-sandbox | false     |

The profile's action-processing event for `Compiling Rust bin policy_contract_test (1 file)` lasted 1.359 seconds. There was **no core Rustc action**, for either `engine_lib` or `engine_lib_test`. Bazel reported 2,145 action-cache hits and a 1.98-second critical path. Total elapsed time was 114.869 seconds including waiting for another Bazel invocation; it is not a compile-time measurement.

This demonstrates isolation of these public-contract test edits. The core unit-test target still exists and still compiles the remaining inline tests.

## Validation

- Built the policy target and core unit-test target successfully.
- Both test targets passed; the core unit suite passed all 11 shards.
- Enumerated `rdeps(//..., //tools/boss/engine/core:engine_lib)` and successfully built all 59 returned targets.
- The first build failed because cached Xcode configuration named 27.1 while the installed toolchain was 27.2. The documented Xcode-upgrade repair (`bazel clean --expunge`) corrected configuration; subsequent builds and tests passed. This was an environmental failure before Rust compilation, unrelated to test relocation.
