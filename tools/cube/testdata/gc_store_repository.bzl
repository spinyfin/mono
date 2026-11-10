"""One platform-independent jj store, downloaded through Bazel's repository cache."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")

_BUILD_FILE = """\
exports_files(["cut"], visibility = ["@@//tools/cube:__pkg__"])

filegroup(
    name = "store",
    srcs = glob(["root/**", "upstream/**", "pool/**"]) + ["cut"],
    visibility = ["@@//tools/cube:__pkg__"],
)
"""

def _gc_store_repositories_impl(ctx):
    pin = json.decode(ctx.read(Label("//tools/cube:testdata/gc_store.json")))
    http_archive(
        name = "cube_gc_store_tree",
        build_file_content = _BUILD_FILE,
        sha256 = pin["sha256"],
        urls = [pin["url"]],
    )

gc_store_repositories = module_extension(implementation = _gc_store_repositories_impl)
