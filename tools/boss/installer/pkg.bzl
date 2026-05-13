"""
Starlark rules for building the Boss installer artifacts.

Defines three rules:
  boss_pkg_payload  — extracts Boss.app.zip into a staged payload directory
  boss_pkg_unsigned — runs pkgbuild to produce Boss-<sha>.pkg (unsigned)
  build_info_rs     — emits a Rust source file with stamped build constants
"""

# ── boss_pkg_payload ──────────────────────────────────────────────────────────

def _boss_pkg_payload_impl(ctx):
    """Extracts the Boss.app archive into a pkgbuild payload directory."""
    output_dir = ctx.actions.declare_directory(ctx.label.name)
    app_archive = ctx.file.app

    ctx.actions.run_shell(
        inputs = [app_archive],
        outputs = [output_dir],
        command = "unzip -q " + app_archive.path + " -d " + output_dir.path,
        mnemonic = "ExtractBossApp",
        progress_message = "Staging Boss.app payload",
    )

    return [DefaultInfo(files = depset([output_dir]))]

boss_pkg_payload = rule(
    implementation = _boss_pkg_payload_impl,
    attrs = {
        "app": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The Boss.app archive (.zip) from //tools/boss/app-macos:Boss.",
        ),
    },
    doc = """
Extracts the Boss.app.zip archive from macos_application into a directory
suitable for use as the --root argument to pkgbuild.

After extraction the directory contains Boss.app/ with all Contents/ including
the bundled binaries in Contents/Resources/bin/.
""",
)

# ── boss_pkg_unsigned ─────────────────────────────────────────────────────────

def _boss_pkg_unsigned_impl(ctx):
    """Runs pkgbuild to produce an unsigned Boss-<sha>.pkg."""
    output_dir = ctx.actions.declare_directory(ctx.label.name)

    # Payload directory (the extracted Boss.app tree)
    payload = ctx.file.payload

    # Pre/postinstall scripts directory — all scripts live in the same dir
    scripts = ctx.files.scripts
    if not scripts:
        fail("boss_pkg_unsigned: scripts attribute must not be empty")
    scripts_dir = scripts[0].dirname

    # ctx.info_file is the non-volatile status file (stable-status.txt), which
    # contains STABLE_* keys.  ctx.version_file is the volatile file and does
    # not carry STABLE_ keys.
    info_file = ctx.info_file

    # Build the shell command.  We avoid .format() to sidestep brace-escaping
    # issues with awk; string concatenation is clearer here.
    command = (
        "set -euo pipefail\n" +
        "SHA=$(grep STABLE_BOSS_GIT_SHA " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$SHA\" ] && SHA=unknown\n" +
        "/usr/bin/pkgbuild \\\n" +
        "    --root " + payload.path + " \\\n" +
        "    --identifier dev.spinyfin.boss.installer \\\n" +
        "    --install-location ~/Applications \\\n" +
        "    --scripts " + scripts_dir + " \\\n" +
        "    --version \"0+${SHA}\" \\\n" +
        "    " + output_dir.path + "/Boss-${SHA}.pkg\n"
    )

    ctx.actions.run_shell(
        inputs = [payload, info_file] + scripts,
        outputs = [output_dir],
        command = command,
        mnemonic = "BossPkgBuild",
        progress_message = "Building unsigned Boss installer .pkg",
        # pkgbuild is macOS-only and must run locally (not in a remote executor)
        execution_requirements = {"local": "1"},
    )

    return [DefaultInfo(files = depset([output_dir]))]

boss_pkg_unsigned = rule(
    implementation = _boss_pkg_unsigned_impl,
    attrs = {
        "payload": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "The staged payload directory from boss_pkg_payload.",
        ),
        "scripts": attr.label(
            mandatory = True,
            allow_files = True,
            doc = "Filegroup of pre/postinstall scripts for pkgbuild --scripts.",
        ),
    },
    doc = """
Builds an unsigned .pkg installer from the staged payload directory.

Output: a directory boss_pkg_unsigned/ containing Boss-<sha>.pkg where <sha>
is STABLE_BOSS_GIT_SHA from the stable workspace status file (ctx.info_file).

The .pkg installs Boss.app to ~/Applications (currentUserHomeDirectory domain,
no admin rights required).  It is unsigned; run release.sh (chore 2) to sign,
notarize, and staple the final artifact.
""",
)

# ── build_info_rs ─────────────────────────────────────────────────────────────

def _build_info_rs_impl(ctx):
    """Emits a Rust source file with stamped build constants."""
    output = ctx.actions.declare_file(ctx.attr.out)
    # ctx.info_file is the non-volatile status file (stable-status.txt)
    info_file = ctx.info_file

    # Read STABLE_BOSS_GIT_SHA and STABLE_BOSS_BUILD_TIME from stable-status.txt
    # and emit a Rust source file with pub constants.  Chore 3 wires these
    # constants into the binaries' --version output.
    command = (
        "set -euo pipefail\n" +
        "SHA=$(grep STABLE_BOSS_GIT_SHA " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "BUILD_TIME=$(grep STABLE_BOSS_BUILD_TIME " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$SHA\" ] && SHA=unknown\n" +
        "[ -z \"$BUILD_TIME\" ] && BUILD_TIME=unknown\n" +
        "printf 'pub const BOSS_GIT_SHA: &str = \"%s\";\\n" +
        "pub const BOSS_BUILD_TIME: &str = \"%s\";\\n' " +
        "\"$SHA\" \"$BUILD_TIME\" > " + output.path + "\n"
    )

    ctx.actions.run_shell(
        inputs = [info_file],
        outputs = [output],
        command = command,
        mnemonic = "BuildInfoRs",
        progress_message = "Generating build_info_generated.rs",
    )

    return [DefaultInfo(files = depset([output]))]

build_info_rs = rule(
    implementation = _build_info_rs_impl,
    attrs = {
        "out": attr.string(
            mandatory = True,
            doc = "Output filename for the generated Rust source file.",
        ),
    },
    doc = """
Generates a Rust source file containing stamped build constants.

Emits:
  pub const BOSS_GIT_SHA: &str = "<sha>";
  pub const BOSS_BUILD_TIME: &str = "<iso8601>";

Chore 3 (app + engine resolution path) will add a dependency on this target
from engine_lib, boss, and bossctl to wire --version output.
""",
)
