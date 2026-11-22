"""Detect Bazel's `--stamp` flag from Starlark.

Starlark rules can read `ctx.info_file` / `ctx.version_file`, but they cannot
observe `--stamp` directly (bazelbuild/bazel#11164). The workaround is a
`config_setting` on `stamp=1`, wrapped in a tiny rule that exposes the bool as
a provider. Rules that should follow `--stamp` take `STAMP_ATTRS` and call
`is_stamping_enabled`.

Convention matches cc_binary / rust_binary:

- `stamp = 1`: always embed workspace-status keys
- `stamp = 0`: never embed; use a constant placeholder
- `stamp = -1` (default): follow `--stamp`
"""

StampSettingInfo = provider(
    doc = "Whether the `--stamp` command-line flag is enabled.",
    fields = {
        "value": "bool: True when `--stamp` is set.",
    },
)

def _stamp_flag_impl(ctx):
    return [StampSettingInfo(value = ctx.attr.value)]

stamp_flag = rule(
    implementation = _stamp_flag_impl,
    attrs = {
        "value": attr.bool(
            doc = "True when the `--stamp` flag is enabled.",
            mandatory = True,
        ),
    },
    provides = [StampSettingInfo],
    doc = "Adapter from config_setting(stamp=1) to StampSettingInfo.",
)

STAMP_ATTRS = {
    "stamp": attr.int(
        default = -1,
        values = [1, 0, -1],
        doc = """Whether to embed workspace-status version keys.

        - 1: always stamp, even under `--nostamp`
        - 0: never stamp; emit the constant placeholder
        - -1: follow the `--[no]stamp` flag (default)
        """,
    ),
    "_stamp_flag": attr.label(
        default = Label("//tools/boss/installer:stamp_flag"),
        providers = [StampSettingInfo],
    ),
}

def is_stamping_enabled(ctx):
    """True when this target should read workspace-status version keys.

    Args:
      ctx: The rule context; must declare `STAMP_ATTRS`.

    Returns:
      True when `stamp = 1`, or when `stamp = -1` and `--stamp` is set.
    """
    stamp_num = ctx.attr.stamp
    if stamp_num > 0:
        return True
    if stamp_num == 0:
        return False
    return ctx.attr._stamp_flag[StampSettingInfo].value

def attrs_with_stamp(attrs):
    """Return `attrs` plus the stamp / _stamp_flag attributes."""
    merged = dict(attrs)
    merged.update(STAMP_ATTRS)
    return merged
