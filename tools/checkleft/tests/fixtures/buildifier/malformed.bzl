# This file is intentionally malformed for spike demonstration purposes.
#
# buildifier --lint=warn would flag it for:
#   - module-docstring: no module-level docstring (must be first statement)
#   - function-docstring: _impl has no docstring
#   - no-effect: the string concatenation on line 11 produces a value that is discarded
#
# buildifier --mode=check would also flag the formatting issues below
# (e.g. trailing whitespace, argument style).

def _my_rule_impl(ctx):
    "unused" + "string"
    return []

my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {},
)
