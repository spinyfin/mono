"""Cache the expensive real-jj operation history used by the GC test."""

def _gc_store_fixture_impl(ctx):
    fixture = ctx.actions.declare_directory(ctx.label.name)
    ctx.actions.run(
        executable = ctx.executable.builder,
        arguments = [ctx.file.jj.path, fixture.path],
        tools = [ctx.file.jj],
        outputs = [fixture],
        mnemonic = "CubeGcFixture",
        progress_message = "Creating the 16,800-operation cube GC fixture",
    )
    return [DefaultInfo(files = depset([fixture]), runfiles = ctx.runfiles(files = [fixture]))]

gc_store_fixture = rule(
    implementation = _gc_store_fixture_impl,
    attrs = {
        "builder": attr.label(executable = True, cfg = "exec", mandatory = True),
        "jj": attr.label(allow_single_file = True, cfg = "exec", mandatory = True),
    },
)
