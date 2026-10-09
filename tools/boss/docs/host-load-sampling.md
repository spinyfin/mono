# Host-load sampling

The engine records how busy the machine it runs on is, so contention can be **measured rather than inferred**. A throughput analysis once had to infer host overload purely from slowdown, because nothing recorded load; at the time the load average was 155–185 on 12 cores with 0 % CPU idle, ~61 of 64 GB used, 23 GB in the compressor and 10.6 of 12 GB swap in use.

This is collection and exposure only. Nothing in dispatch reads these numbers.

## What is recorded

Once per interval (default 60 s) the engine takes one sample:

| Group        | Fields                                                   | Source                                                                                                                  |
| ------------ | -------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Load average | 1 / 5 / 15 min                                           | `sysctl vm.loadavg`                                                                                                     |
| CPU          | busy / user / system / idle %                            | delta of `host_statistics(HOST_CPU_LOAD_INFO)` ticks since the previous sample (nice counts as user; busy = 100 − idle) |
| Memory       | total, used, compressor, free, available, pressure level | `hw.memsize`, `host_statistics64(HOST_VM_INFO64)`, `kern.memorystatus_level`                                            |
| Swap         | total, used                                              | `sysctl vm.swapusage`                                                                                                   |
| CPUs         | logical CPU count                                        | `sysctl hw.logicalcpu`                                                                                                  |

_Used_ is Activity Monitor's figure — app (anonymous minus purgeable) + wired + compressed — and _available_ is `total − used`. _Pressure level_ is the kernel's own `kern.memorystatus_level`: percent of memory available, so **low means pressure**.

The first sample after engine start has no CPU percentages (there is no previous tick reading to diff); those fields are `NULL`. A `NULL` anywhere means "not measured", never zero.

## Syscalls only

Sampling never spawns a process — no `vm_stat`, `memory_pressure`, `top`, `ps`, `sysctl` binary or `uptime`. Forking on a saturated, swapping host costs tens of milliseconds and adds the load being measured. Each tick is a handful of direct kernel calls (`sysctl(2)` with MIBs resolved once, plus two Mach `host_statistics*` calls). The syscall layer sits behind the `HostProbe` trait in `tools/boss/engine/host-load`, so tests use fakes.

A failing call leaves its fields `NULL` for that sample, logs **once** at WARN with the errno (or Mach return code), and the sampler keeps running. It cannot affect dispatch or the engine.

## Where it lands

- **Gauges** (current value only; `bossctl metrics list --prefix host.`): `host.load_avg_{1m,5m,15m}_x100`, `host.cpu_{busy,user,system,idle}_pct_x100`, `host.mem_{total,used,compressor,free,available}_bytes`, `host.mem_pressure_level_pct`, `host.swap_{total,used}_bytes`, `host.logical_cpus`. Gauges are integers, so fractional values are fixed-point ×100 (`15550` = load 155.50, `9950` = 99.5 %).
- **Retained series**: table `host_load_samples` in `state.db`, one row per sample, full `REAL` precision. `ts_ms` is epoch milliseconds, **UTC**. The framework's gauges only keep the latest value, so the series is what answers "what was the host doing at 14:07?".

## Settings

In `<state root>/settings.toml`; read once at engine start (restart to apply). Write the keys quoted, as the engine itself does, or use the nested-table form:

```toml
"host_load.sample_interval_seconds" = 60   # default 60, clamped to 5..=3600
"host_load.retention_days" = 7             # default 7, clamped to 1..=365
```

Samples older than the retention window are pruned at startup and then hourly.

## Querying

```sh
bossctl metrics host                  # latest sample + last hour, local time
bossctl metrics host --window 6h
bossctl --json metrics host --window 2d
```

It reads `state.db` directly, so it works with the engine wedged. Text output is in local time and thins long windows to ~120 rows; `--json` returns every sample with `ts_ms` in UTC epoch milliseconds:

```json
{
  "window": { "requested": "1h", "since_ms": 0, "until_ms": 0 },
  "current": { "ts_ms": 0, "load_avg_1m": 0.0, "cpu_busy_pct": 0.0, "...": "..." },
  "current_age_ms": 0,
  "samples": [{ "ts_ms": 0, "...": "..." }]
}
```
