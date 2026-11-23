"""Bundle Homebrew tmux and its non-system macOS dylibs for Bazel runfiles."""

import os
import shutil
import subprocess
import sys


def list_non_system_deps(macho_path: str, is_dylib: bool) -> list[str]:
    out = subprocess.check_output(["otool", "-L", macho_path], text=True)
    lines = out.splitlines()[1:]
    if is_dylib and lines:
        # First entry of a dylib is its own LC_ID_DYLIB.
        lines = lines[1:]
    deps = []
    for line in lines:
        dep = line.strip().split(" (compatibility version", 1)[0]
        if not dep or dep.startswith("/usr/lib/") or dep.startswith("/System/"):
            continue
        deps.append(dep)
    return deps


def resolve_dep(dep: str, parent_real_path: str) -> tuple[str, str]:
    if dep.startswith("@loader_path/"):
        rel = dep[len("@loader_path/") :]
        raw = os.path.normpath(os.path.join(os.path.dirname(parent_real_path), rel))
    else:
        raw = dep
    return raw, os.path.realpath(raw)


def main() -> None:
    src_bin, out_bin, out_lib_dir = sys.argv[1:4]
    shutil.copy2(src_bin, out_bin)
    os.chmod(out_bin, 0o755)

    os.makedirs(out_lib_dir, exist_ok=True)
    copied: dict[str, str] = {}  # resolved_src_path -> basename
    watched = {src_bin}
    queue: list[tuple[str, str]] = []

    for dep in list_non_system_deps(out_bin, False):
        raw_dep, resolved = resolve_dep(dep, src_bin)
        watched.add(raw_dep)
        watched.add(resolved)
        base = os.path.basename(resolved)
        if resolved not in copied:
            copied[resolved] = base
            queue.append((resolved, base))
        subprocess.check_call(
            ["install_name_tool", "-change", dep, "@loader_path/lib/" + base, out_bin],
            stdout=subprocess.DEVNULL,
        )

    while queue:
        resolved_src, base = queue.pop(0)
        dst_lib = os.path.join(out_lib_dir, base)
        shutil.copy2(resolved_src, dst_lib)
        os.chmod(dst_lib, 0o755)
        subprocess.check_call(
            ["install_name_tool", "-id", "@loader_path/" + base, dst_lib],
            stdout=subprocess.DEVNULL,
        )
        for dep in list_non_system_deps(dst_lib, True):
            raw_dep, dep_resolved = resolve_dep(dep, resolved_src)
            watched.add(raw_dep)
            watched.add(dep_resolved)
            dep_base = os.path.basename(dep_resolved)
            if dep_resolved not in copied:
                copied[dep_resolved] = dep_base
                queue.append((dep_resolved, dep_base))
            subprocess.check_call(
                ["install_name_tool", "-change", dep, "@loader_path/" + dep_base, dst_lib],
                stdout=subprocess.DEVNULL,
            )
        subprocess.check_call(
            ["codesign", "--force", "--sign", "-", dst_lib],
            stdout=subprocess.DEVNULL,
        )

    subprocess.check_call(
        ["codesign", "--force", "--sign", "-", out_bin],
        stdout=subprocess.DEVNULL,
    )
    for watched_path in sorted(watched):
        print(watched_path)


if __name__ == "__main__":
    main()
