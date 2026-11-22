"""NativeLink dashboard format and publication boundary (stdlib only)."""

import json
from pathlib import Path
import sys

DASHBOARDS = ("nativelink-overview.json", "nativelink-workers.json")
PROVIDER = "nativelink.yaml"
DIRECTORY = "/etc/grafana/nativelink-dashboards"
# This exact allowlisted provider is also the privileged installer's policy.
# Changing its scope requires updating the root-owned validator on headcrash.
PROVISIONING = """apiVersion: 1
providers:
  - name: "NativeLink Dashboards"
    orgId: 1
    folder: "NativeLink"
    type: file
    disableDeletion: false
    updateIntervalSeconds: 30
    allowUiUpdates: false
    options:
      path: /etc/grafana/nativelink-dashboards
"""


def canonical(value):
    return json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n"


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def parse(text):
    return json.loads(text, object_pairs_hook=unique_keys)


def validate_dashboard(dashboard, uid):
    if dashboard.get("uid") != uid:
        raise ValueError(f"expected dashboard uid {uid}")
    if "id" in dashboard or "version" in dashboard:
        raise ValueError("remove volatile dashboard id/version")
    seen = set()

    def panels(items):
        occupied = []
        for panel in items:
            identity = panel["id"]
            if type(identity) is not int or identity in seen:
                raise ValueError(f"invalid or duplicate panel id: {identity}")
            seen.add(identity)
            source = panel.get("datasource")
            if not isinstance(source, dict) or not source.get("uid") or not source.get("type"):
                raise ValueError(f"panel {identity}: missing datasource")
            grid = panel["gridPos"]
            x, y, w, h = (grid[key] for key in ("x", "y", "w", "h"))
            if any(type(v) is not int for v in (x, y, w, h)) or min(x, y) < 0 or min(w, h) <= 0 or x + w > 24:
                raise ValueError(f"panel {identity}: invalid gridPos")
            for other, ox, oy, ow, oh in occupied:
                if x < ox + ow and ox < x + w and y < oy + oh and oy < y + h:
                    raise ValueError(f"panels {other} and {identity} overlap")
            occupied.append((identity, x, y, w, h))
            # Collapsed rows have their own layout; ids are dashboard-wide.
            panels(panel.get("panels", []))

    if not dashboard.get("panels"):
        raise ValueError("dashboard has no panels")
    panels(dashboard["panels"])


def normalize_dashboard(dashboard, uid):
    """Strip volatile fields, add built-in datasources on row/text panels, and validate."""
    dashboard.pop("id", None)
    dashboard.pop("version", None)

    def fill_datasources(items):
        for panel in items:
            if not panel.get("datasource"):
                if panel.get("type") in ("row", "text"):
                    panel["datasource"] = {"type": "grafana", "uid": "-- Grafana --"}
                else:
                    target_ds = [
                        t["datasource"]
                        for t in panel.get("targets", [])
                        if isinstance(t, dict) and isinstance(t.get("datasource"), dict)
                    ]
                    if target_ds:
                        panel["datasource"] = target_ds[0]
            fill_datasources(panel.get("panels", []))

    fill_datasources(dashboard.get("panels", []))
    validate_dashboard(dashboard, uid)
    return canonical(dashboard)


def validate_bundle(bundle):
    if set(bundle) != {*DASHBOARDS, PROVIDER}:
        raise ValueError("publication must contain exactly the two NativeLink dashboards and provider")
    if bundle[PROVIDER] != PROVISIONING:
        raise ValueError("provider must be read-only and scoped to the NativeLink folder and directory")
    for name in DASHBOARDS:
        validate_dashboard(bundle[name], Path(name).stem)
    canonical(bundle)  # Reject non-finite numbers anywhere in the export.


def load_bundle(directory):
    bundle = {PROVIDER: (directory / PROVIDER).read_text()}
    for name in DASHBOARDS:
        text = (directory / name).read_text()
        bundle[name] = parse(text)
        def sorted_keys(value):
            if isinstance(value, dict):
                if list(value) != sorted(value):
                    raise ValueError(f"{name}: sort JSON object keys recursively")
                for child in value.values():
                    sorted_keys(child)
            elif isinstance(value, list):
                for child in value:
                    sorted_keys(child)
        sorted_keys(bundle[name])
    validate_bundle(bundle)
    return bundle


def resolve_normalize_target(raw_arg, dashboards_dir=None):
    target = Path(raw_arg)
    if target.parent == Path("."):
        base = Path(__file__).resolve().parent if dashboards_dir is None else Path(dashboards_dir)
        target = base / target.name
    target = target.resolve()
    if target.name not in DASHBOARDS:
        raise ValueError(f"target filename must be one of {', '.join(DASHBOARDS)}")
    return target


if __name__ == "__main__":
    if sys.argv[1:2] == ["--normalize"]:
        if len(sys.argv) != 3:
            sys.exit("usage: validate.py --normalize <target>")
        try:
            target = resolve_normalize_target(sys.argv[2])
        except ValueError as err:
            sys.exit(str(err))
        dashboard = parse(sys.stdin.read())
        target.write_text(normalize_dashboard(dashboard, target.stem))
        print(f"Normalized and validated {target}")
        print("Run bin/checkleft fix to format JSON before committing.")
    else:
        bundle = load_bundle(Path(__file__).resolve().parent)
        if sys.argv[1:] == ["--bundle"]:
            print(canonical(bundle), end="")
        elif sys.argv[1:]:
            sys.exit("usage: validate.py [--bundle | --normalize <target>]")
        else:
            print("Validated both NativeLink dashboards and provisioning")
