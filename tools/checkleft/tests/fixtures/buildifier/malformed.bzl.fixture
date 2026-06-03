# This file is intentionally malformed for testing the buildifier check.
#
# buildifier --lint=warn flags it for:
#   - module-docstring: no module-level docstring (must be first statement)
#   - function-docstring: _impl has no docstring
#   - no-effect: the string concatenation on line 12 produces a value that is discarded
#
# buildifier --mode=check also flags the formatting issues below
# (e.g. trailing whitespace, argument style).

def _my_rule_impl(ctx):
    "unused" + "string"
    return []

my_rule = rule(
    implementation = _my_rule_impl,
    attrs = {},
)
