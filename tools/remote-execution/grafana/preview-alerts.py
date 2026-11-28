#!/usr/bin/env python3
"""Preview provisioned alert thresholds with synthetic inputs; never installs rules.

Run on the Grafana host with PyYAML available:
  python3 preview-alerts.py nativelink-hub.yaml http://localhost:3000

The PromQL operators and Grafana threshold models come from the provisioning
file. Only source series are replaced, so no production thresholds are changed.
Preview evaluates the firing condition; it does not wait out the rule's `for`.
"""
import copy
import json
import re
import sys
import urllib.request

import yaml

rules = yaml.safe_load(open(sys.argv[1]))["groups"][0]["rules"]
values = {
    "nativelink_hub_store_health_timeouts_total": 4,
    "nativelink_hub_status_up": 0,
    "nativelink_hub_start_time_seconds": "time() - 600",
    "nativelink_hub_expected_workers": 10,
    "nativelink_hub_connected_workers": 8,
    "nativelink_pool_admin_up": 1,
    "nativelink_hub_command_timeouts_total": 18,
    "nativelink_hub_action_kills_total": 24,
    "nativelink_hub_worker_kill_ack_evictions_total": 1,
    "nativelink_hub_scheduler_action_timeouts_total": 4,
    "nativelink_hub_compressed_upload_decoded_size_errors_total": 7,
    "nativelink_hub_cache_used_bytes": 97,
    "nativelink_hub_cache_filesystem_bytes": 100,
    "nativelink_hub_journal_watch_up": 0,
    "nativelink_hub_status_last_probe_timestamp_seconds": "time() - 120",
}
def preview(rule, inputs, scrape_up=0, expected=1):
    data = copy.deepcopy(rule["data"])
    expr = data[0]["model"]["expr"]
    # A synthetic linear counter exercises increase() over the original window.
    expr = re.sub(r"increase\((\w+)\[(\d+)m\]\)",
                  lambda m: f"increase((vector(time() * {inputs[m[1]]} / {int(m[2]) * 60}))[{m[2]}m:15s])", expr)
    for name, value in inputs.items():
        expr = re.sub(rf"\b{name}\b", f"vector({value})", expr)
    expr = expr.replace('up{job="nativelink-pool"}', f'vector({scrape_up})')
    data[0]["model"]["expr"] = expr
    request = urllib.request.Request(
        sys.argv[2] + "/api/v1/eval",
        data=json.dumps({"data": data, "condition": rule["condition"]}).encode(),
        headers={"Content-Type": "application/json"},
    )
    response = json.load(urllib.request.urlopen(request, timeout=30))
    result = response["results"][rule["condition"]]
    assert result["status"] == 200 and not result.get("error"), response
    outputs = [v for frame in result["frames"] for column in frame["data"]["values"] for v in column]
    assert outputs and all(v == expected for v in outputs), response
    state = "Alerting" if expected else "Normal"
    print(f"{rule['uid']}: {state} threshold={expected}; synthetic query: {expr}")


for rule in rules:
    preview(rule, values)
    if rule["uid"] == "nl-hub-kills-timeouts-burst":
        preview(rule, dict(values, nativelink_hub_command_timeouts_total=0))
        preview(rule, dict(values, nativelink_hub_action_kills_total=0))
    if rule["uid"] == "nl-hub-monitoring-unavailable":
        healthy = dict(values, nativelink_hub_journal_watch_up=1,
                       nativelink_hub_status_last_probe_timestamp_seconds="time()")
        preview(rule, healthy, scrape_up=1, expected=0)
        preview(rule, healthy, scrape_up=0)
        preview(rule, dict(healthy, nativelink_hub_journal_watch_up=0), scrape_up=1)
        preview(rule, dict(healthy, nativelink_hub_status_last_probe_timestamp_seconds="time() - 120"), scrape_up=1)
