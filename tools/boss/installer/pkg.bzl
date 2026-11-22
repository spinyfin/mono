"""
Starlark rules for building the Boss installer artifacts.

Defines four rules:
  boss_pkg_payload        — extracts Boss.app.zip into a staged payload directory
  boss_pkg_unsigned       — runs pkgbuild to produce Boss-<sha>.pkg (unsigned)
  build_info_rs           — emits a Rust source file with stamped build constants
  boss_short_version_plist — emits a plist fragment with both CFBundleShortVersionString
                             (numeric STABLE_BOSS_BASE_VERSION, e.g. "1.0.4") and
                             BossFullVersion (full STABLE_BOSS_VERSION, e.g.
                             "1.0.4-dev-f3be785") so the About panel shows the full
                             version while keeping CFBundleVersion Apple-compliant.

`build_info_rs` and `boss_short_version_plist` follow `--stamp`: unstamped builds
write a constant placeholder and do not read `ctx.info_file`, so a new `boss-v*`
tag does not change their action keys (or those of engine_lib / cli / tests).
Release builds pass `--config=release` (`--stamp`) and embed the tag-derived
version.
"""

load(":stamp.bzl", "attrs_with_stamp", "is_stamping_enabled")

# Placeholder version compiled into non-release (unstamped) builds. Constant so
# the generated file's action key is independent of git tags.
UNSTAMPED_BOSS_VERSION = "0.0.0-dev"

# Apple-required numeric CFBundleVersion / CFBundleShortVersionString when
# unstamped. BossFullVersion carries `-dev-` so UpdateChecker treats local/CI
# app builds as dev builds and will not auto-install a published release over
# them.
UNSTAMPED_BOSS_BASE_VERSION = "0.0.0"
UNSTAMPED_BOSS_FULL_VERSION = "0.0.0-dev-unstamped"

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
    """Runs pkgbuild + productbuild to produce an unsigned Boss-<sha>.pkg."""
    output_dir = ctx.actions.declare_directory(ctx.label.name)

    # Payload directory (the extracted Boss.app tree)
    payload = ctx.file.payload

    # Distribution XML — enables currentUserHomeDirectory domain so Installer
    # uses the runtime user's ~/Applications rather than a build-time-expanded
    # absolute path.
    distribution = ctx.file.distribution

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
    #
    # Two-step build mirrors release.sh:
    #   1. pkgbuild → BossComponent.pkg  (component package, --install-location /Applications)
    #   2. productbuild --distribution   → Boss-<sha>.pkg  (distribution package)
    #
    # Using /Applications (not ~/Applications) avoids baking the builder's $HOME
    # into the component metadata.  productbuild + distribution.xml with
    # enable_currentUserHome="true" remaps /Applications → ~/Applications at
    # install time on the target machine.
    command = (
        "set -euo pipefail\n" +
        # Anchored on '^STABLE_BOSS_GIT_SHA ' (trailing space) so this does not
        # also match the STABLE_BOSS_GIT_SHA_FULL line below it — an
        # unanchored grep would return both lines, giving $SHA an embedded
        # newline that word-splits the unquoted Boss-${SHA}.pkg argument
        # further down and breaks productbuild's argv.
        "SHA=$(grep '^STABLE_BOSS_GIT_SHA ' " + info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$SHA\" ] && SHA=unknown\n" +
        # Step 1: component package (intermediate; named to match distribution.xml pkg-ref)
        "/usr/bin/pkgbuild \\\n" +
        "    --root " + payload.path + " \\\n" +
        "    --identifier dev.spinyfin.boss.installer \\\n" +
        "    --install-location /Applications \\\n" +
        "    --scripts " + scripts_dir + " \\\n" +
        "    --version \"0+${SHA}\" \\\n" +
        "    " + output_dir.path + "/BossComponent.pkg\n" +
        # Step 2: distribution package wrapping the component with domain metadata
        "/usr/bin/productbuild \\\n" +
        "    --distribution " + distribution.path + " \\\n" +
        "    --package-path " + output_dir.path + " \\\n" +
        "    " + output_dir.path + "/Boss-${SHA}.pkg\n"
    )

    ctx.actions.run_shell(
        inputs = [payload, distribution, info_file] + scripts,
        outputs = [output_dir],
        command = command,
        mnemonic = "BossPkgBuild",
        progress_message = "Building unsigned Boss installer .pkg",
        # pkgbuild/productbuild are macOS-only, so this needs a Mac, but not
        # the Mac that invoked the build: a remote macOS worker can run it and
        # its result can be cached. It still runs outside the sandbox.
        execution_requirements = {
            "no-sandbox": "1",
            "requires-darwin": "",
        },
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
        "distribution": attr.label(
            mandatory = True,
            allow_single_file = True,
            doc = "distribution.xml for productbuild — sets currentUserHomeDirectory domain.",
        ),
    },
    doc = """
Builds an unsigned .pkg installer from the staged payload directory.

Output: a directory boss_pkg_unsigned/ containing Boss-<sha>.pkg where <sha>
is STABLE_BOSS_GIT_SHA from the stable workspace status file (ctx.info_file).
Also contains BossComponent.pkg (intermediate component package).

Two-step build: pkgbuild produces BossComponent.pkg with --install-location
/Applications; productbuild wraps it with distribution.xml which sets
enable_currentUserHome="true" so Installer.app resolves /Applications relative
to the runtime user's home, not the builder's $HOME.

The .pkg installs Boss.app to ~/Applications (currentUserHomeDirectory domain,
no admin rights required).  It is unsigned; run release.sh (chore 2) to sign,
notarize, and staple the final artifact.
""",
)

# ── build_info_rs ─────────────────────────────────────────────────────────────

def _build_info_rs_source(version):
    return (
        "pub const BOSS_VERSION: &str = \"%s\";\n" % version +
        "#[allow(dead_code)]\n" +
        "pub const BOSS_GIT_SHA: &str = \"unknown\";\n" +
        "#[allow(dead_code)]\n" +
        "pub const BOSS_BUILD_TIME: &str = \"unknown\";\n"
    )

def _build_info_rs_impl(ctx):
    """Emits a Rust source file with stamped (or placeholder) build constants."""
    output = ctx.actions.declare_file(ctx.attr.out)

    # This file is compiled into engine_lib, boss, and bossctl. Reading
    # stable-status.txt here — even only the tag-derived base version — makes
    # every new boss-v* tag a new action key for those crates and the 40–80
    # tests downstream of them. Unstamped builds therefore write a constant
    # placeholder with ctx.actions.write and do not take ctx.info_file as an
    # input. Release builds (`--config=release` / `--stamp`) read
    # STABLE_BOSS_BASE_VERSION from stable-status.txt.
    #
    # SHA and wall-clock time stay the literal "unknown" even when stamped:
    # those values live in build_provenance_rs (a leaf crate only the engine
    # binaries link). See that rule's doc comment.
    if is_stamping_enabled(ctx):
        info_file = ctx.file.status_file if ctx.attr.status_file else ctx.info_file
        tmpl = ctx.actions.declare_file(ctx.attr.out + ".tmpl")
        ctx.actions.write(
            output = tmpl,
            content = _build_info_rs_source("@BOSS_VERSION@"),
        )
        command = (
            "set -euo pipefail\n" +
            "VERSION=$(grep '^STABLE_BOSS_BASE_VERSION ' " + info_file.path +
            " | cut -d' ' -f2 2>/dev/null || true)\n" +
            "[ -z \"$VERSION\" ] && VERSION=unknown\n" +
            "sed \"s|@BOSS_VERSION@|${VERSION}|g\" " + tmpl.path + " > " + output.path + "\n"
        )
        ctx.actions.run_shell(
            inputs = [tmpl, info_file],
            outputs = [output],
            command = command,
            mnemonic = "BuildInfoRs",
            progress_message = "Generating build_info_generated.rs",
        )
    else:
        ctx.actions.write(
            output = output,
            content = _build_info_rs_source(UNSTAMPED_BOSS_VERSION),
        )

    return [DefaultInfo(files = depset([output]))]

build_info_rs = rule(
    implementation = _build_info_rs_impl,
    attrs = attrs_with_stamp({
        "out": attr.string(
            mandatory = True,
            doc = "Output filename for the generated Rust source file.",
        ),
        "status_file": attr.label(
            allow_single_file = True,
            doc = "Optional test fixture overriding ctx.info_file when stamped.",
        ),
    }),
    doc = """
Generates a Rust source file containing build constants.

Unstamped (`--nostamp`, the default for check / developer builds):
  pub const BOSS_VERSION: &str = "0.0.0-dev";
  The action does not read stable-status.txt, so a new boss-v* tag does not
  change this file or anything that compiles it in.

Stamped (`--stamp` / `--config=release`):
  pub const BOSS_VERSION: &str = "<STABLE_BOSS_BASE_VERSION>";  // e.g. "1.0.4"

BOSS_GIT_SHA and BOSS_BUILD_TIME are the literal "unknown" in both cases.
Per-commit identity lives in `build_provenance_rs` (leaf crate, engine
binaries only). The user-facing release version in the app bundle is
`boss_short_version_plist`, which is also stamp-gated.
""",
)

# ── build_provenance_rs ───────────────────────────────────────────────────────

def _build_provenance_rs_impl(ctx):
    """Emits a Rust source file with the version, sha, dirty flag and build time."""
    output = ctx.actions.declare_file(ctx.attr.out)

    # ctx.info_file (stable-status.txt) carries the commit-derived facts;
    # ctx.version_file (volatile-status.txt) carries the wall-clock build
    # time. They are read here — and ONLY here, never by build_info_rs above
    # — so the per-commit stamping stays out of engine_lib's own source and
    # lives in its own small, dedicated leaf crate (`boss-build-provenance`)
    # that only the `engine` rust_binary targets link. That is what keeps the
    # churn out of engine_lib's Bazel action cache: the output here changes
    # every commit, so everything downstream of it rebuilds every commit, and
    # the downstream set is deliberately just the two engine binaries
    # (guarded by //tools/boss/engine/core:stamped_provenance_isolation_test).
    # See the `LiveStatusDebugReport.engine_build_sha` doc comment for how
    # the values appear in the live-status debug report.
    #
    # VERSION is STABLE_BOSS_VERSION (`1.0.N` on a release tag, `1.0.N-dev-<sha>`
    # otherwise) when `--stamp` is set, and UNSTAMPED_BOSS_VERSION (`0.0.0-dev`)
    # when unstamped so the tag-derived version only reaches release binaries.
    # Empty / `0.0.0` when stamped fall back to `unknown` — never the
    # Cargo-crate placeholder. SHA and dirty are always populated.
    #
    # BUILD_TIME follows the same gate, and an unstamped build does not take
    # volatile-status.txt as an input at all. The output of an unstamped build
    # is then a function of the commit alone. With the time in it, the output
    # differed every time the action ran, and the action runs again at an
    # unchanged commit whenever stable-status.txt changes (a new boss-v* tag
    # moves STABLE_BOSS_VERSION) or a machine has not built that commit yet.
    # Each of those relinked both engine binaries and re-ran
    # build-provenance_test, and no remote cache entry could ever match,
    # because the file's content is part of a remote action's key. The engine
    # reports its binary's modification time when the stamp says `unknown`
    # (`build_info::build_time`).
    inputs = [ctx.info_file]
    if is_stamping_enabled(ctx):
        inputs.append(ctx.version_file)
        version_cmd = (
            "VERSION=$(grep '^STABLE_BOSS_VERSION ' " + ctx.info_file.path +
            " | cut -d' ' -f2 2>/dev/null || true)\n" +
            "if [ -z \"$VERSION\" ] || [ \"$VERSION\" = \"0.0.0\" ]; then VERSION=unknown; fi\n"
        )
        build_time_cmd = (
            "BUILD_TIME=$(grep '^BOSS_BUILD_TIME ' " + ctx.version_file.path +
            " | cut -d' ' -f2 2>/dev/null || true)\n" +
            "[ -z \"$BUILD_TIME\" ] && BUILD_TIME=unknown\n"
        )
    else:
        version_cmd = "VERSION=\"" + UNSTAMPED_BOSS_VERSION + "\"\n"
        build_time_cmd = "BUILD_TIME=unknown\n"

    command = (
        "set -euo pipefail\n" +
        version_cmd +
        "SHA=$(grep '^STABLE_BOSS_GIT_SHA_FULL ' " + ctx.info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ -z \"$SHA\" ] && SHA=unknown\n" +
        "DIRTY=$(grep '^STABLE_BOSS_GIT_DIRTY ' " + ctx.info_file.path +
        " | cut -d' ' -f2 2>/dev/null || true)\n" +
        "[ \"$DIRTY\" = \"true\" ] && DIRTY=true || DIRTY=false\n" +
        build_time_cmd +
        "printf 'pub const VERSION: &str = \"%s\";\\n" +
        "pub const GIT_SHA: &str = \"%s\";\\n" +
        "pub const GIT_DIRTY: bool = %s;\\n" +
        "pub const BUILD_TIME: &str = \"%s\";\\n' " +
        "\"$VERSION\" \"$SHA\" \"$DIRTY\" \"$BUILD_TIME\" > " + output.path + "\n"
    )

    ctx.actions.run_shell(
        inputs = inputs,
        outputs = [output],
        command = command,
        mnemonic = "BuildProvenanceRs",
        progress_message = "Generating build_provenance_generated.rs",
    )

    return [DefaultInfo(files = depset([output]))]

build_provenance_rs = rule(
    implementation = _build_provenance_rs_impl,
    attrs = attrs_with_stamp({
        "out": attr.string(
            mandatory = True,
            doc = "Output filename for the generated Rust source file.",
        ),
    }),
    doc = """
Generates a Rust source file containing the engine's build provenance.

Emits:
  pub const VERSION: &str = "0.0.0-dev" (unstamped) or "<1.0.N or 1.0.N-dev-<sha>, else unknown>" (stamped);
  pub const GIT_SHA: &str = "<full commit sha>";
  pub const GIT_DIRTY: bool = <true|false>;
  pub const BUILD_TIME: &str = "unknown" (unstamped) or "<ISO-8601 UTC>" (stamped);

Unlike `build_info_rs` (which deliberately stamps "unknown" for the sha to
protect engine_lib's compile cache), this rule feeds a dedicated,
otherwise-empty `boss-build-provenance` leaf crate — see that crate's
BUILD.bazel and doc comment. The output changes on every commit, so every
action downstream of it re-runs on every commit; the crate is therefore
linked only by the `engine` rust_binary targets, which hand the values to
engine_lib at startup (`build_info::install_provenance`). If engine_lib
linked the crate directly, a docs-only commit would recompile engine_lib,
every `rust_test(crate = ":engine_lib")` binary, `boss` and `bossctl`, and
re-execute their tests — see
tools/boss/docs/investigations/docs-only-pr-reexecutes-engine-tests-2026-10-21.md.
""",
)

# ── boss_short_version_plist ──────────────────────────────────────────────────

def _boss_version_plist_xml(base_version, full_version):
    return (
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n" +
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\"" +
        " \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n" +
        "<plist version=\"1.0\"><dict>\n" +
        "<key>CFBundleShortVersionString</key><string>%s</string>\n" % base_version +
        "<key>CFBundleVersion</key><string>%s</string>\n" % base_version +
        "<key>BossFullVersion</key><string>%s</string>\n" % full_version +
        "</dict></plist>\n"
    )

def _boss_short_version_plist_impl(ctx):
    """Emits a plist fragment with version keys, stamp-gated."""
    output = ctx.actions.declare_file(ctx.attr.out)

    # Same stamp gate as build_info_rs: mac-app-build and developer builds
    # must not rebuild the app bundle when a new boss-v* tag is cut. The
    # placeholder BossFullVersion contains "-dev-" so UpdateChecker treats
    # those builds as dev builds.
    #
    # CFBundleVersion and CFBundleShortVersionString must be period-separated
    # non-negative integers (plisttool enforces Apple's requirement). The full
    # version including the "-dev-<sha>" suffix goes in the custom
    # BossFullVersion key; the macOS app reads it via Bundle.main and passes
    # it to orderFrontStandardAboutPanel(options:).
    if is_stamping_enabled(ctx):
        info_file = ctx.file.status_file if ctx.attr.status_file else ctx.info_file
        tmpl = ctx.actions.declare_file(ctx.attr.out + ".tmpl")
        ctx.actions.write(
            output = tmpl,
            content = _boss_version_plist_xml("@BOSS_BASE_VERSION@", "@BOSS_FULL_VERSION@"),
        )
        command = (
            "set -euo pipefail\n" +
            "V=$(grep '^STABLE_BOSS_VERSION ' " + info_file.path +
            " | cut -d' ' -f2 2>/dev/null || true)\n" +
            "B=$(grep '^STABLE_BOSS_BASE_VERSION ' " + info_file.path +
            " | cut -d' ' -f2 2>/dev/null || true)\n" +
            "[ -z \"$V\" ] && V=dev\n" +
            "[ -z \"$B\" ] && B=0.0.0\n" +
            "sed -e \"s|@BOSS_BASE_VERSION@|${B}|g\" -e \"s|@BOSS_FULL_VERSION@|${V}|g\" " +
            tmpl.path + " > " + output.path + "\n"
        )
        ctx.actions.run_shell(
            inputs = [tmpl, info_file],
            outputs = [output],
            command = command,
            mnemonic = "BossShortVersionPlist",
            progress_message = "Generating Boss version plist",
        )
    else:
        ctx.actions.write(
            output = output,
            content = _boss_version_plist_xml(
                UNSTAMPED_BOSS_BASE_VERSION,
                UNSTAMPED_BOSS_FULL_VERSION,
            ),
        )

    return [DefaultInfo(files = depset([output]))]

boss_short_version_plist = rule(
    implementation = _boss_short_version_plist_impl,
    attrs = attrs_with_stamp({
        "out": attr.string(
            mandatory = True,
            doc = "Output filename for the generated plist fragment.",
        ),
        "status_file": attr.label(
            allow_single_file = True,
            doc = "Optional test fixture overriding ctx.info_file when stamped.",
        ),
    }),
    doc = """
Generates a minimal Info.plist fragment with version keys.

Unstamped (`--nostamp`):
  CFBundleShortVersionString = CFBundleVersion = "0.0.0"
  BossFullVersion            = "0.0.0-dev-unstamped"
  The action does not read stable-status.txt.

Stamped (`--stamp` / `--config=release`):
  CFBundleShortVersionString = STABLE_BOSS_BASE_VERSION (e.g. "1.0.4")  — numeric only
  CFBundleVersion            = STABLE_BOSS_BASE_VERSION (e.g. "1.0.4")  — numeric only
  BossFullVersion            = STABLE_BOSS_VERSION      (e.g. "1.0.4-dev-f3be785")

CFBundleVersion and CFBundleShortVersionString must be numeric-only (plisttool
enforces Apple's requirement). The full version including the "-dev-<sha>"
suffix goes in BossFullVersion (custom key, not validated); the macOS app
reads it at runtime and passes it to orderFrontStandardAboutPanel(options:).
""",
)
