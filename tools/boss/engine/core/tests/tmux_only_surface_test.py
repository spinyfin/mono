"""Guard the tmux-only local worker architecture across production surfaces.

In-file tests are scanned too. Historical design documents are not production
surfaces. Remote detached SSH workers, the frontend InterruptWorkerPane RPC,
and viewer-only attach/detach/focus requests are deliberately allowed.

The spawn ordering scan is a coarse textual tripwire, not a control-flow proof.
The spawn_flow behavioral test local_spawn_requires_successfully_persisted_identity
is the primary enforcement of intent persistence before any tmux command.
"""

import os
from pathlib import Path
import re
import sys


REMOVED = (
    "SpawnWorkerPane", "spawnWorkerPane", "spawn_worker_pane",
    "ReleaseWorkerPane", "releaseWorkerPane",
    "UpdateWorkerShellPid", "updateWorkerShellPid", "update_worker_shell_pid",
    "WorkerPaneDied", "workerPaneDied", "worker_pane_died",
    "ReportWorkerSpawnFailed", "reportWorkerSpawnFailed", "report_worker_spawn_failed",
    "WorkerProcessKiller", "killForegroundProcessTree", "WorkerPaneDeathReason",
    "SendToPane", "sendToPane", "send_to_pane",
    "EngineToAppRequest::InterruptWorkerPane",
    "EngineToAppResponse::InterruptWorkerPane", "interruptWorkerPane",
    "TmuxHostingPools", "tmux_hosting_pools", "tmux_hosting_enabled_for",
    "dispatch_hosting_stamp", "tmuxHosting", "workers.tmux_hosting",
)
REMOVED_PATTERN = re.compile("|".join(map(re.escape, REMOVED)))
OPTIONAL_HOST = re.compile(r"Option\s*<\s*(?:Arc\s*<\s*)?TmuxWorkerHost\b")
VIEWER_REQUESTS = {
    "AttachCoordinatorPane", "AttachWorkerPane", "DetachWorkerPane",
    "FocusWorkerPane", "ListHostedPanes", "OpenDocument", "RevealWorkItem",
}


def source_files(root):
    """Read raw source once, without repeating long runfiles paths per line."""
    sources = {}
    for directory, extension in (
        ("engine/core/src", ".rs"),
        ("protocol/src", ".rs"),
        ("engine/driver/src", ".rs"),
        ("bossctl/src", ".rs"),
        ("app-macos/Sources", ".swift"),
    ):
        count = 0
        # Runfiles may use directory as well as file symlinks. A missing or
        # unreadable root must fail rather than pass a vacuous scan.
        def fail_walk(error):
            raise error

        for parent, _, files in os.walk(root / directory, followlinks=True, onerror=fail_walk):
            for name in files:
                if name.endswith(extension):
                    path = Path(parent) / name
                    sources[path.relative_to(root).as_posix()] = path.read_text(encoding="utf-8")
                    count += 1
        if not count:
            raise RuntimeError(f"No {extension} sources under {directory}")
    return sources


def violations(sources):
    errors = []
    for path, content in sources.items():
        for match in REMOVED_PATTERN.finditer(content):
            identifier = match.group()
            # Permit only stale-key diagnostics and TOML rejection fixtures.
            # Registry references through the constant are checked below too.
            line_text = content[content.rfind("\n", 0, match.start()) + 1:
                                content.find("\n", match.end())].strip()
            if identifier == "workers.tmux_hosting" and path == "engine/core/src/settings.rs":
                if (line_text.startswith("///")
                        or line_text == 'const TMUX_HOSTING_SETTING: &str = "workers.tmux_hosting";'
                        or line_text == 'const TMUX_HOSTING_REMOVED_MESSAGE: &str = "remove `workers.tmux_hosting`; local workers are always tmux-hosted";'
                        or (content[match.start() - 2:match.start()] == '\\"'
                            and content[match.end():match.end() + 5] == '\\" = ')):
                    continue
            line = content.count("\n", 0, match.start()) + 1
            errors.append(f"{path}:{line}: removed identifier {identifier}")
        if path.startswith("engine/core/src/") and OPTIONAL_HOST.search(content):
            errors.append(f"{path}: an optional TmuxWorkerHost permits a local spawn without identity")

    settings = sources["engine/core/src/settings.rs"]
    registry = re.search(r"pub const REGISTRY\s*:[^=]*=\s*&\[(.*?)^\];", settings, re.M | re.S)
    if not registry or re.search(r"workers\.tmux_hosting|\bTMUX_HOSTING_SETTING\b", registry[1]):
        errors.append("settings REGISTRY must not expose the removed tmux hosting key")

    spawn = sources["engine/core/src/spawn_flow.rs"]
    declaration = re.search(r"pub struct StartWorkerInput\b[^\{]*\{(.*?)^\}", spawn, re.M | re.S)
    if not declaration or not re.search(r"pub\s+tmux_host\s*:\s*TmuxWorkerHost\s*,", declaration[1]):
        errors.append("StartWorkerInput.tmux_host must be a required TmuxWorkerHost")
    # Match the call, not the earlier trait declaration. Behavioral tests in
    # spawn_flow also require a successful intent write before any tmux command.
    intent = re.search(r"\.record_tmux_spawn_intent\s*\(", spawn)
    create = re.search(r"\.new_session\s*\(", spawn)
    if not intent or not create or intent.start() >= create.start():
        errors.append("durable tmux intent must precede session creation")

    if "NotTmuxHosted" in sources["engine/core/src/app/tmux_teardown.rs"]:
        errors.append("local teardown must not return the remote-only NotTmuxHosted outcome")
    if "tmux_hosted" in sources["engine/core/src/worker_registry.rs"]:
        errors.append("WorkerRegistry must not retain a per-pane hosting-mode bit")

    protocol = sources["protocol/src/engine_app.rs"]
    enum = re.search(r"pub enum EngineToAppRequest\s*\{(.*?)^\}", protocol, re.M | re.S)
    variants = set(re.findall(r"^    ([A-Z]\w*)\b", enum[1], re.M)) if enum else set()
    if variants != VIEWER_REQUESTS:
        errors.append(f"EngineToAppRequest must remain viewer-only; found {sorted(variants)}")
    return errors


def verify_guard(sources):
    """Negative controls prove the scan rejects regressions, not just today's tree."""
    spawn_path = "engine/core/src/spawn_flow.rs"
    # Structural checks need these real declarations. Restrict the controls to
    # them so every spelling need not rescan the entire production tree.
    sources = {path: sources[path] for path in (
        spawn_path,
        "engine/core/src/settings.rs",
        "engine/core/src/app/tmux_teardown.rs",
        "engine/core/src/worker_registry.rs",
        "protocol/src/engine_app.rs",
    )}
    spawn = sources[spawn_path]
    mutations = [
        (spawn_path, spawn.replace("pub tmux_host: TmuxWorkerHost,", "pub tmux_host: Option<TmuxWorkerHost>,")),
        (spawn_path, spawn.replace(".record_tmux_spawn_intent(", ".skip_intent(")),
        (spawn_path, spawn.replace(".record_tmux_spawn_intent(", ".new_session(); store.record_tmux_spawn_intent(", 1)),
        ("engine/core/src/app/tmux_teardown.rs", "NotTmuxHosted"),
        ("engine/core/src/worker_registry.rs", "tmux_hosted: bool"),
        ("protocol/src/engine_app.rs", "pub enum EngineToAppRequest {\n    SpawnWorkerPane,\n}"),
    ]
    settings_path = "engine/core/src/settings.rs"
    for key in ('"workers.tmux_hosting"', "TMUX_HOSTING_SETTING"):
        entry = f'\n    SettingSpec {{ key: {key}, description: "regression", default_enabled: false }},'
        mutations.append((settings_path, sources[settings_path].replace(
            "pub const REGISTRY: &[SettingSpec] = &[",
            "pub const REGISTRY: &[SettingSpec] = &[" + entry, 1)))
    mutations.append((settings_path, sources[settings_path] + '\nconst REINTRODUCED: &str = "workers.tmux_hosting";\n'))
    # Exercise each removed spelling in each scanned production language.
    for identifier in REMOVED:
        for path in ("protocol/src/regression.rs", "app-macos/Sources/Regression.swift"):
            mutations.append((path, identifier))
    for path, content in mutations:
        mutated = dict(sources)
        mutated[path] = content
        if not violations(mutated):
            raise AssertionError(f"Guard accepted regression in {path}: {content[:120]}")

    allowed = dict(sources)
    allowed["engine/core/src/remote_detached.rs"] = """
        RemoteDetachedWorker NotTmuxHosted
        FrontendRequest::InterruptWorkerPane
        AttachWorkerPane DetachWorkerPane FocusWorkerPane ListHostedPanes
        release_worker_pane
    """
    if violations(allowed):
        raise AssertionError("Guard rejected remote detached workers or viewer-only operations")


def main():
    root = Path(os.environ["TEST_SRCDIR"]) / os.environ.get("TEST_WORKSPACE", "_main") / "tools/boss"
    sources = source_files(root)
    errors = violations(sources)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    verify_guard(sources)
    print(f"OK: tmux-only local-pane invariant holds across {len(sources)} source files.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
