"""NativeLink Prometheus configuration validator (stdlib only)."""

import ipaddress
import os
from pathlib import Path
import shutil
import subprocess
import sys

CONFIG_NAME = "prometheus.yml"
CONFIG_PATH = Path("/etc/prometheus/prometheus.yml")
REQUIRED_JOBS = ("prometheus", "nativelink", "nativelink-pool", "node")
LOOPBACK_ONLY_JOBS = {"prometheus", "nativelink", "nativelink-pool"}
TAILSCALE_NET = ipaddress.IPv4Network("100.64.0.0/10")


def _strip_comment(line):
    in_quote = None
    escaped = False
    for idx, ch in enumerate(line):
        if escaped:
            escaped = False
            continue
        if ch == "\\" and in_quote == '"':
            escaped = True
            continue
        if ch in ('"', "'"):
            if in_quote is None:
                in_quote = ch
            elif in_quote == ch:
                in_quote = None
        elif ch == "#" and in_quote is None:
            return line[:idx]
    return line


def _split_flow(text):
    items = []
    current = []
    depth = 0
    in_quote = None
    escaped = False
    for ch in text:
        if escaped:
            current.append(ch)
            escaped = False
            continue
        if ch == "\\" and in_quote == '"':
            current.append(ch)
            escaped = True
            continue
        if ch in ('"', "'"):
            if in_quote is None:
                in_quote = ch
            elif in_quote == ch:
                in_quote = None
            current.append(ch)
            continue
        if in_quote is None:
            if ch in "[{":
                depth += 1
            elif ch in "]}":
                depth -= 1
            elif ch == "," and depth == 0:
                items.append("".join(current).strip())
                current = []
                continue
        current.append(ch)
    tail = "".join(current).strip()
    if tail:
        items.append(tail)
    return items


def _parse_scalar(text):
    text = text.strip()
    if len(text) >= 2 and text[0] == text[-1] and text[0] in ('"', "'"):
        return text[1:-1]
    if text in ("true", "True", "TRUE"):
        return True
    if text in ("false", "False", "FALSE"):
        return False
    if text in ("null", "~"):
        return None
    if text.startswith("[") and text.endswith("]"):
        inner = text[1:-1].strip()
        return [_parse_scalar(item) for item in _split_flow(inner)] if inner else []
    if text.startswith("{") and text.endswith("}"):
        inner = text[1:-1].strip()
        result = {}
        if not inner:
            return result
        for pair in _split_flow(inner):
            if ":" not in pair:
                raise ValueError(f"invalid flow mapping entry: {pair!r}")
            key, val = pair.split(":", 1)
            k = _parse_scalar(key)
            if k in result:
                raise ValueError(f"duplicate YAML key: {k}")
            result[k] = _parse_scalar(val)
        return result
    try:
        return int(text)
    except ValueError:
        return text


def _split_kv(text):
    in_quote = None
    escaped = False
    for idx, ch in enumerate(text):
        if escaped:
            escaped = False
            continue
        if ch == "\\" and in_quote == '"':
            escaped = True
            continue
        if ch in ('"', "'"):
            if in_quote is None:
                in_quote = ch
            elif in_quote == ch:
                in_quote = None
        elif ch == ":" and in_quote is None and (idx + 1 == len(text) or text[idx + 1] == " "):
            return text[:idx].strip(), text[idx + 1 :].strip()
    return None


def parse_yaml(text):
    """Parse the Prometheus YAML subset into Python dicts/lists/scalars."""
    lines = []
    for raw in text.splitlines():
        if "\t" in raw[: len(raw) - len(raw.lstrip(" \t"))]:
            raise ValueError("tabs are not allowed in YAML indentation")
        cleaned = _strip_comment(raw).rstrip()
        if cleaned.strip():
            indent = len(cleaned) - len(cleaned.lstrip(" "))
            lines.append((indent, cleaned.lstrip(" ")))

    def parse_block(idx, min_indent):
        if idx >= len(lines):
            return None, idx
        indent, content = lines[idx]
        if indent < min_indent:
            return None, idx
        if content.startswith("- ") or content == "-":
            items = []
            seq_indent = indent
            while idx < len(lines) and lines[idx][0] == seq_indent and (
                lines[idx][1].startswith("- ") or lines[idx][1] == "-"
            ):
                rest = lines[idx][1][1:].strip()
                idx += 1
                if not rest:
                    child, idx = parse_block(idx, seq_indent + 1)
                    items.append(child)
                elif _split_kv(rest) is not None:
                    # Inline mapping started on the "- " line; parse at rest's column.
                    rest_indent = seq_indent + 2
                    lines.insert(idx, (rest_indent, rest))
                    child, idx = parse_mapping(idx, rest_indent)
                    items.append(child)
                else:
                    items.append(_parse_scalar(rest))
            return items, idx
        return parse_mapping(idx, indent)

    def parse_mapping(idx, map_indent):
        mapping = {}
        while idx < len(lines) and lines[idx][0] == map_indent:
            content = lines[idx][1]
            if content.startswith("- ") or content == "-":
                break
            kv = _split_kv(content)
            if kv is None:
                raise ValueError(f"expected mapping entry, got: {content!r}")
            key_raw, val_raw = kv
            key = _parse_scalar(key_raw)
            if key in mapping:
                raise ValueError(f"duplicate YAML key: {key}")
            idx += 1
            if val_raw:
                mapping[key] = _parse_scalar(val_raw)
            else:
                if idx < len(lines) and lines[idx][0] >= map_indent:
                    if lines[idx][0] == map_indent and not (
                        lines[idx][1].startswith("- ") or lines[idx][1] == "-"
                    ):
                        mapping[key] = None
                    else:
                        child, idx = parse_block(idx, map_indent)
                        mapping[key] = child
                else:
                    mapping[key] = None
        return mapping, idx

    if not lines:
        raise ValueError("empty Prometheus config")
    value, next_idx = parse_block(0, 0)
    if next_idx != len(lines):
        raise ValueError(f"unexpected trailing YAML at line {lines[next_idx][1]!r}")
    return value


def _split_host_port(target):
    if not isinstance(target, str) or ":" not in target:
        raise ValueError(f"invalid target {target!r}: expected host:port")
    host, port_str = target.rsplit(":", 1)
    if not host or not port_str.isdigit():
        raise ValueError(f"invalid target {target!r}: expected host:port")
    port = int(port_str)
    if not (1 <= port <= 65535):
        raise ValueError(f"invalid port in target {target!r}")
    return host, port


def validate_config(text):
    """Validate Prometheus YAML structure, jobs, targets, and security invariants."""
    config = parse_yaml(text)
    if not isinstance(config, dict):
        raise ValueError("Prometheus config must be a YAML mapping")
    if not isinstance(config.get("global"), dict):
        raise ValueError("Prometheus config missing 'global' mapping")
    scrape_configs = config.get("scrape_configs")
    if not isinstance(scrape_configs, list) or not scrape_configs:
        raise ValueError("Prometheus config missing non-empty 'scrape_configs'")

    jobs = {}
    for job in scrape_configs:
        if not isinstance(job, dict) or not isinstance(job.get("job_name"), str):
            raise ValueError("each scrape_configs entry must have a string job_name")
        name = job["job_name"]
        if name in jobs:
            raise ValueError(f"duplicate job_name: {name}")
        jobs[name] = job

        static_configs = job.get("static_configs")
        if not isinstance(static_configs, list) or not static_configs:
            raise ValueError(f"job {name!r}: expected non-empty static_configs")
        for sc in static_configs:
            if not isinstance(sc, dict):
                raise ValueError(f"job {name!r}: static_configs entry must be a mapping")
            targets = sc.get("targets")
            if not isinstance(targets, list) or not targets:
                raise ValueError(f"job {name!r}: static_configs entry missing targets")
            for target in targets:
                host, _ = _split_host_port(target)
                if host in ("0.0.0.0", "::", "[::]", "*"):
                    raise ValueError(f"job {name!r}: forbidden wildcard target {target!r}")
                if name in LOOPBACK_ONLY_JOBS and host not in ("127.0.0.1", "localhost"):
                    raise ValueError(f"job {name!r}: target {target!r} must be loopback")

    for required in REQUIRED_JOBS:
        if required not in jobs:
            raise ValueError(f"missing required scrape job: {required}")

    seen_instances = set()
    seen_targets = set()
    for sc in jobs["node"]["static_configs"]:
        labels = sc.get("labels")
        if not isinstance(labels, dict):
            raise ValueError("node job entry missing labels")
        instance = labels.get("instance")
        platform = labels.get("platform")
        pool = labels.get("pool")
        if not isinstance(instance, str) or not instance:
            raise ValueError("node job entry missing instance label")
        if instance in seen_instances:
            raise ValueError(f"duplicate node instance label: {instance}")
        seen_instances.add(instance)
        if platform not in ("macos", "linux"):
            raise ValueError(f"node {instance!r}: platform must be 'macos' or 'linux'")
        if pool != platform:
            raise ValueError(f"node {instance!r}: pool {pool!r} must match platform {platform!r}")
        for target in sc["targets"]:
            if target in seen_targets:
                raise ValueError(f"duplicate node target: {target}")
            seen_targets.add(target)
            host, port = _split_host_port(target)
            if port != 9100:
                raise ValueError(f"node {instance!r}: expected port 9100, got {port}")
            addr = ipaddress.IPv4Address(host)
            if platform == "macos":
                if labels.get("worker_id") != instance:
                    raise ValueError(f"macOS node {instance!r}: worker_id must match instance")
                if addr not in TAILSCALE_NET:
                    raise ValueError(f"macOS node {instance!r}: {host} must be a Tailscale IPv4")
            elif host != "127.0.0.1" and addr not in TAILSCALE_NET:
                raise ValueError(f"Linux node {instance!r}: {host} must be 127.0.0.1 or Tailscale IPv4")

    return config


def run_promtool(config_file, promtool_bin=None, syntax_only=False):
    binary = promtool_bin or os.environ.get("PROMTOOL") or shutil.which("promtool")
    if not binary:
        for candidate in ("/usr/local/bin/promtool", "/usr/bin/promtool"):
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                binary = candidate
                break
    if not binary:
        raise FileNotFoundError("promtool binary not found")
    cmd = [binary, "check", "config"]
    if syntax_only:
        cmd.append("--syntax-only")
    cmd.append(str(config_file))
    subprocess.run(cmd, check=True)


if __name__ == "__main__":
    args = sys.argv[1:]
    use_promtool = False
    emit_config = False
    target_path = Path(__file__).resolve().parent / CONFIG_NAME
    while args:
        arg = args.pop(0)
        if arg == "--promtool":
            use_promtool = True
        elif arg == "--print":
            emit_config = True
        elif not arg.startswith("-"):
            target_path = Path(arg)
        else:
            sys.exit("usage: validate.py [--promtool] [--print] [path/to/prometheus.yml]")
    text = target_path.read_text()
    validate_config(text)
    if use_promtool:
        run_promtool(target_path)
    if emit_config:
        sys.stdout.write(text)
    else:
        print(f"Validated {target_path}")
